//! Tests: the full blob storage pipeline.
//!
//! Scenario: pack small blobs → upload to storage → fetch back via cache pipeline.
//! Uses opendal local filesystem backend to avoid needing real S3.

use afs_db::*;
use afs_store::backend::BackendConfig;
use afs_store::cache::CacheConfig;
use afs_store::pack::{PackEntryData, PACK_THRESHOLD};

async fn setup_store(tmpdir: &tempfile::TempDir) -> (afs_store::BlobStore, sqlx::SqlitePool) {
    let pool = schema::open_db(":memory:").await.unwrap();
    let store_dir = tmpdir.path().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();

    let backend = BackendConfig::Fs {
        root: store_dir.to_str().unwrap().to_string(),
    };
    let cache = CacheConfig { memory_capacity: 1024 * 1024, disk_dir: None, disk_capacity: 0 };

    let store = afs_store::BlobStore::new(&backend, &cache, pool.clone())
        .await
        .unwrap();

    (store, pool)
}

// ── Pack upload and fetch ───────────────────────────────────────

#[tokio::test]
async fn pack_small_blobs_and_fetch_individually() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let (store, pool) = setup_store(&tmpdir).await;

    // Create small blobs
    let blobs = vec![
        PackEntryData { oid: "a".repeat(40), data: b"content of file A".to_vec() },
        PackEntryData { oid: "b".repeat(40), data: b"content of file B".to_vec() },
        PackEntryData { oid: "c".repeat(40), data: vec![0u8; 512] },
    ];

    // Upload as a pack
    let pack_id = store.put_pack(&blobs).await.unwrap();
    assert!(!pack_id.is_empty());

    // Verify pack_index was populated
    let entry_a = packs::get_pack_entry(&pool, &"a".repeat(40)).await.unwrap();
    assert!(entry_a.is_some());

    // Fetch each blob back through the store pipeline
    for blob in &blobs {
        let data = store.get_blob(&blob.oid).await.unwrap();
        assert_eq!(data.as_ref(), blob.data.as_slice(), "blob {} mismatch", blob.oid);
    }
}

#[tokio::test]
async fn large_blob_uploaded_directly() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let (store, _pool) = setup_store(&tmpdir).await;

    let large_data = vec![42u8; PACK_THRESHOLD + 1];
    let oid = "d".repeat(40);

    store.put_blob(&oid, &large_data).await.unwrap();

    // Fetch it back
    let fetched = store.get_blob(&oid).await.unwrap();
    assert_eq!(fetched.len(), large_data.len());
    assert_eq!(fetched.as_ref(), large_data.as_slice());
}

#[tokio::test]
async fn cache_serves_repeated_reads() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let (store, _pool) = setup_store(&tmpdir).await;

    let oid = "e".repeat(40);
    store.put_blob(&oid, b"cached data").await.unwrap();

    // First read (from S3/fs)
    let first = store.get_blob(&oid).await.unwrap();
    assert_eq!(first.as_ref(), b"cached data");

    // Second read should come from cache
    assert!(store.is_cached(&oid));
    let second = store.get_blob(&oid).await.unwrap();
    assert_eq!(second.as_ref(), b"cached data");
}

#[tokio::test]
async fn missing_blob_returns_error() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let (store, _pool) = setup_store(&tmpdir).await;

    let result = store.get_blob(&"f".repeat(40)).await;
    assert!(result.is_err());
}

// ── Pack format behavior ────────────────────────────────────────

#[test]
fn pack_with_many_entries_roundtrips() {
    use afs_store::pack;

    let entries: Vec<PackEntryData> = (0..100)
        .map(|i| PackEntryData {
            oid: format!("{:040x}", i),
            data: format!("content of blob {}", i).into_bytes(),
        })
        .collect();

    let (pack_bytes, index) = pack::write_pack(&entries).unwrap();

    // Verify integrity
    assert!(pack::verify_pack(&pack_bytes).unwrap());

    // Parse index from pack
    let parsed_index = pack::parse_pack_index(&pack_bytes).unwrap();
    assert_eq!(parsed_index.len(), 100);

    // Spot check a few entries
    for i in [0, 42, 99] {
        let idx = &index[i];
        let data = pack::read_blob_from_pack(&pack_bytes, idx.offset, idx.comp_size).unwrap();
        assert_eq!(data, entries[i].data);
    }
}

#[test]
fn corrupted_pack_fails_verification() {
    use afs_store::pack;

    let entries = vec![PackEntryData {
        oid: "a".repeat(40),
        data: b"test".to_vec(),
    }];

    let (mut pack_bytes, _) = pack::write_pack(&entries).unwrap();

    // Corrupt a byte
    pack_bytes[20] ^= 0xFF;

    assert!(!pack::verify_pack(&pack_bytes).unwrap());
}

// ── LFS pointer detection ───────────────────────────────────────

#[test]
fn lfs_pointer_detected_in_realistic_content() {
    use afs_indexer::parse_lfs_pointer;

    // Real-world LFS pointer content
    let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 132423\n";
    let parsed = parse_lfs_pointer(pointer.as_bytes()).unwrap();
    assert_eq!(parsed.size, 132423);
    assert_eq!(parsed.oid.len(), 64);

    // Regular source code should NOT be detected as LFS
    assert!(parse_lfs_pointer(b"fn main() {}").is_none());
    assert!(parse_lfs_pointer(b"{ \"key\": \"value\" }").is_none());
    assert!(parse_lfs_pointer(b"").is_none());
}
