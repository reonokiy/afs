//! Tests: LFS object storage — upload to S3/local, fetch back, cache behavior.
//!
//! Uses opendal local FS backend. Does not test LFS batch API
//! (that requires a real or mocked HTTP server).

use afs_store::backend::BackendConfig;
use afs_store::cache::{BlobCache, CacheConfig};
use bytes::Bytes;

async fn setup_lfs(tmpdir: &tempfile::TempDir) -> (opendal::Operator, BlobCache, reqwest::Client) {
    let store_dir = tmpdir.path().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();

    let config = BackendConfig::Fs {
        root: store_dir.to_str().unwrap().to_string(),
    };
    let op = afs_store::backend::create_operator(&config).unwrap();
    let cache = BlobCache::new(&CacheConfig { memory_capacity: 1024 * 1024, disk_dir: None, disk_capacity: 0 })
        .await
        .unwrap();
    let client = reqwest::Client::new();

    (op, cache, client)
}

#[tokio::test]
async fn upload_and_fetch_lfs_object() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (op, cache, client) = setup_lfs(&tmp).await;

    let oid = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let content = b"this is a large binary asset pretending to be LFS content";

    // Upload
    afs_store::lfs::upload_lfs_object(oid, content, &op)
        .await
        .unwrap();

    // Fetch (no batch API, just S3/local)
    let data = afs_store::lfs::fetch_lfs_object(oid, &cache, &op, None, &client)
        .await
        .unwrap();

    assert_eq!(data.as_ref(), content);
}

#[tokio::test]
async fn fetch_caches_lfs_object() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (op, cache, client) = setup_lfs(&tmp).await;

    let oid = "1111111111111111111111111111111111111111111111111111111111111111";
    let content = b"cached lfs data";

    afs_store::lfs::upload_lfs_object(oid, content, &op)
        .await
        .unwrap();

    // First fetch — from storage
    let _ = afs_store::lfs::fetch_lfs_object(oid, &cache, &op, None, &client)
        .await
        .unwrap();

    // Should now be in cache
    assert!(cache.contains(oid));

    // Second fetch — from cache
    let data = afs_store::lfs::fetch_lfs_object(oid, &cache, &op, None, &client)
        .await
        .unwrap();
    assert_eq!(data.as_ref(), content);
}

#[tokio::test]
async fn missing_lfs_object_without_batch_api_returns_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (op, cache, client) = setup_lfs(&tmp).await;

    let oid = "0000000000000000000000000000000000000000000000000000000000000000";

    // No batch API URL → should fail with clear error
    let result = afs_store::lfs::fetch_lfs_object(oid, &cache, &op, None, &client).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not in S3") || err_msg.contains("no LFS server"),
        "error should mention S3 miss or missing server URL, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn lfs_key_uses_prefix_bucketing() {
    // Verify the key format: lfs/{first 2 chars}/{full oid}
    let oid = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let key = afs_store::backend::lfs_key(oid);
    assert_eq!(key, format!("lfs/ab/{}", oid));
}

#[tokio::test]
async fn upload_multiple_lfs_objects_and_fetch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (op, cache, client) = setup_lfs(&tmp).await;

    let objects = vec![
        ("aa11111111111111111111111111111111111111111111111111111111111111", "model weights v1"),
        ("bb22222222222222222222222222222222222222222222222222222222222222", "training data chunk"),
        ("cc33333333333333333333333333333333333333333333333333333333333333", "image asset"),
    ];

    for (oid, content) in &objects {
        afs_store::lfs::upload_lfs_object(oid, content.as_bytes(), &op)
            .await
            .unwrap();
    }

    for (oid, content) in &objects {
        let data = afs_store::lfs::fetch_lfs_object(oid, &cache, &op, None, &client)
            .await
            .unwrap();
        assert_eq!(data.as_ref(), content.as_bytes());
    }
}
