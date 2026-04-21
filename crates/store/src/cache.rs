use std::path::PathBuf;
use anyhow::{Context, Result};
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    RecoverMode,
};
use serde::Deserialize;
use tracing::info;

/// Default memory cache capacity: 256 MB.
const DEFAULT_MEMORY_CAPACITY: usize = 256 * 1024 * 1024;

/// Default disk cache capacity: 2 GB.
const DEFAULT_DISK_CAPACITY: usize = 2 * 1024 * 1024 * 1024;

/// Cache configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// In-memory cache capacity in bytes.
    #[serde(default = "default_memory_capacity")]
    pub memory_capacity: usize,
    /// Disk cache directory. None = memory-only (no disk layer).
    #[serde(default)]
    pub disk_dir: Option<PathBuf>,
    /// Disk cache capacity in bytes.
    #[serde(default = "default_disk_capacity")]
    pub disk_capacity: usize,
}

fn default_memory_capacity() -> usize {
    DEFAULT_MEMORY_CAPACITY
}

fn default_disk_capacity() -> usize {
    DEFAULT_DISK_CAPACITY
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_capacity: DEFAULT_MEMORY_CAPACITY,
            disk_dir: None,
            disk_capacity: DEFAULT_DISK_CAPACITY,
        }
    }
}

/// Two-level blob cache: L1 in-memory + L2 on-disk (optional).
#[derive(Clone)]
pub struct BlobCache {
    inner: HybridCache<String, Bytes>,
}

impl BlobCache {
    /// Create a new blob cache.
    /// If `disk_dir` is configured, enables persistent disk cache that survives restarts.
    pub async fn new(config: &CacheConfig) -> Result<Self> {
        let builder = HybridCacheBuilder::new()
            .memory(config.memory_capacity)
            .storage();

        let cache = if let Some(ref dir) = config.disk_dir {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create disk cache dir: {}", dir.display()))?;

            let device = FsDeviceBuilder::new(dir)
                .with_capacity(config.disk_capacity)
                .build()
                .context("build disk cache device")?;
            let engine = BlockEngineConfig::new(device);

            builder
                .with_engine_config(engine)
                .with_recover_mode(RecoverMode::Quiet)
                .build()
                .await
                .context("build hybrid cache with disk")?
        } else {
            builder
                .build()
                .await
                .context("build hybrid cache (memory-only)")?
        };

        info!(
            memory_mb = config.memory_capacity / (1024 * 1024),
            disk_mb = config.disk_dir.as_ref().map(|_| config.disk_capacity / (1024 * 1024)),
            disk_dir = ?config.disk_dir,
            "blob cache initialized"
        );

        Ok(Self { inner: cache })
    }

    /// Try to get a blob from cache (checks memory, then disk).
    pub async fn get(&self, oid: &str) -> Option<Bytes> {
        match self.inner.get(oid).await {
            Ok(Some(entry)) => Some(entry.value().clone()),
            _ => None,
        }
    }

    /// Insert a blob into cache (goes to memory, evictions spill to disk).
    pub fn insert(&self, oid: String, data: Bytes) {
        self.inner.insert(oid, data);
    }

    /// Check if a blob is in cache without loading it.
    pub fn contains(&self, oid: &str) -> bool {
        self.inner.contains(oid)
    }
}
