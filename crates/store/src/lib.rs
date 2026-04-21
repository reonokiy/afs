pub mod backend;
pub mod cache;
pub mod lfs;
pub mod pack;

use anyhow::{Context, Result};
use bytes::Bytes;
use opendal::Operator;
use sqlx::SqlitePool;
use tracing::debug;

use crate::backend::BackendConfig;
use crate::cache::{BlobCache, CacheConfig};

/// Threshold for multipart upload: 5 MB.
const MULTIPART_THRESHOLD: usize = 5 * 1024 * 1024;

/// The main blob store: cache → pack_index → backend (S3/GCS/Azure/FS).
pub struct BlobStore {
    cache: BlobCache,
    operator: Operator,
    pool: SqlitePool,
    /// Reusable HTTP client for LFS and other HTTP operations.
    client: reqwest::Client,
}

impl BlobStore {
    /// Create a new BlobStore.
    pub async fn new(
        backend_config: &BackendConfig,
        cache_config: &CacheConfig,
        pool: SqlitePool,
    ) -> Result<Self> {
        let operator = backend::create_operator(backend_config)?;
        let cache = BlobCache::new(cache_config).await?;
        let client = reqwest::Client::new();

        Ok(Self {
            cache,
            operator,
            pool,
            client,
        })
    }

    /// Get the shared reqwest::Client.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Get a blob by OID. Goes through: cache → pack_index → backend.
    pub async fn get_blob(&self, oid: &str) -> Result<Bytes> {
        // L1/L2: Check cache first
        if let Some(data) = self.cache.get(oid).await {
            debug!(%oid, "cache hit");
            return Ok(data);
        }

        // Check pack_index for this OID
        if let Some(pack_entry) = afs_db::packs::get_pack_entry(&self.pool, oid).await? {
            debug!(%oid, pack_id = %pack_entry.pack_id, "fetching from pack via range read");
            let data = self.fetch_from_pack(&pack_entry).await?;
            let bytes = Bytes::from(data);
            self.cache.insert(oid.to_string(), bytes.clone());
            return Ok(bytes);
        }

        // Try direct blob key from backend
        let key = backend::blob_key(oid);
        debug!(%oid, %key, "fetching blob from backend");
        match self.operator.read(&key).await {
            Ok(buf) => {
                let bytes = buf.to_bytes();
                self.cache.insert(oid.to_string(), bytes.clone());
                Ok(bytes)
            }
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                anyhow::bail!("blob {} not found in backend", oid);
            }
            Err(e) => Err(e).context(format!("fetch blob {}", oid)),
        }
    }

    /// Fetch a blob from a pack file using range read.
    async fn fetch_from_pack(&self, entry: &afs_db::packs::PackEntry) -> Result<Vec<u8>> {
        let key = backend::pack_key(&entry.pack_id);
        let offset = entry.offset as u64;
        let range_size = pack::ENTRY_HEADER_SIZE as u64 + entry.comp_size as u64;

        let buf = self
            .operator
            .read_with(&key)
            .range(offset..offset + range_size)
            .await
            .context("backend range read from pack")?;

        let range_data = buf.to_vec();
        pack::read_blob_from_range(&range_data, entry.comp_size as u32)
    }

    /// Upload a single blob to the backend.
    /// For blobs >= 5 MB, uses concurrent multipart upload for better throughput.
    pub async fn put_blob(&self, oid: &str, data: &[u8]) -> Result<()> {
        let key = backend::blob_key(oid);
        let bytes = Bytes::from(data.to_vec());

        if data.len() >= MULTIPART_THRESHOLD {
            // Use concurrent multipart upload for large blobs
            self.operator
                .write_with(&key, bytes.clone())
                .concurrent(8)
                .await
                .context("multipart upload blob to backend")?;
        } else {
            self.operator
                .write(&key, bytes.clone())
                .await
                .context("upload blob to backend")?;
        }

        self.cache.insert(oid.to_string(), bytes);
        Ok(())
    }

    /// Upload a pack file to the backend and update the local pack_index.
    pub async fn put_pack(&self, entries: &[pack::PackEntryData]) -> Result<String> {
        let (pack_bytes, index_entries) = pack::write_pack(entries)?;
        let pack_id = content_hash_hex(&pack_bytes);
        let key = backend::pack_key(&pack_id);

        self.operator
            .write(&key, pack_bytes)
            .await
            .context("upload pack to backend")?;

        // Update local pack_index
        let db_entries: Vec<afs_db::packs::PackEntry> = index_entries
            .into_iter()
            .map(|e| afs_db::packs::PackEntry {
                oid: e.oid,
                pack_id: pack_id.clone(),
                offset: e.offset as i64,
                comp_size: e.comp_size as i64,
                raw_size: e.raw_size as i64,
            })
            .collect();

        afs_db::packs::bulk_insert_pack_entries(&self.pool, &db_entries).await?;

        Ok(pack_id)
    }

    /// Get multiple blobs by OID in a batch. Checks cache first, then does a
    /// single batched pack_index query, and falls back to individual backend reads.
    /// Returns results in arbitrary order as (oid, data) pairs.
    pub async fn get_blobs_batch(&self, oids: &[&str]) -> Result<Vec<(String, Bytes)>> {
        let mut results = Vec::with_capacity(oids.len());
        let mut remaining = Vec::new();

        // Check cache first
        for &oid in oids {
            if let Some(data) = self.cache.get(oid).await {
                debug!(%oid, "batch: cache hit");
                results.push((oid.to_string(), data));
            } else {
                remaining.push(oid);
            }
        }

        if remaining.is_empty() {
            return Ok(results);
        }

        // Batch pack_index lookup
        let remaining_refs: Vec<&str> = remaining.to_vec();
        let pack_entries =
            afs_db::packs::get_pack_entries_batch(&self.pool, &remaining_refs).await?;

        let mut still_remaining: std::collections::HashSet<&str> =
            remaining.iter().copied().collect();

        for entry in &pack_entries {
            let data = self.fetch_from_pack(entry).await?;
            let bytes = Bytes::from(data);
            self.cache.insert(entry.oid.clone(), bytes.clone());
            results.push((entry.oid.clone(), bytes));
            still_remaining.remove(entry.oid.as_str());
        }

        // Fall back to individual backend reads for anything not in packs
        for oid in still_remaining {
            let key = backend::blob_key(oid);
            match self.operator.read(&key).await {
                Ok(buf) => {
                    let bytes = buf.to_bytes();
                    self.cache.insert(oid.to_string(), bytes.clone());
                    results.push((oid.to_string(), bytes));
                }
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                    debug!(%oid, "batch: blob not found in backend, skipping");
                }
                Err(e) => return Err(e).context(format!("batch fetch blob {}", oid)),
            }
        }

        Ok(results)
    }

    /// Get an LFS object by its SHA-256 OID.
    /// Tries: cache → S3 lfs/ prefix → LFS batch API fallback.
    pub async fn get_lfs_object(
        &self,
        lfs_oid: &str,
        lfs_server_url: Option<&str>,
    ) -> Result<Bytes> {
        lfs::fetch_lfs_object(
            lfs_oid,
            &self.cache,
            &self.operator,
            lfs_server_url,
            &self.client,
        )
        .await
    }

    /// Upload an LFS object to the backend.
    pub async fn put_lfs_object(&self, oid: &str, data: &[u8]) -> Result<()> {
        lfs::upload_lfs_object(oid, data, &self.operator).await
    }

    /// Check if a blob exists in cache.
    pub fn is_cached(&self, oid: &str) -> bool {
        self.cache.contains(oid)
    }

    /// Get the opendal operator (for direct backend access in sync operations).
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
}

/// Entry header size constant re-export for range read calculations.
pub use pack::ENTRY_HEADER_SIZE;

fn content_hash_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}
