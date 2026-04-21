//! Tests: LFS batch API client with a mock HTTP server.
//!
//! Spins up a local TCP server that speaks the LFS batch protocol,
//! then verifies fetch_lfs_object falls through to the batch API correctly.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use afs_store::backend::BackendConfig;
use afs_store::cache::{BlobCache, CacheConfig};

const TEST_OID: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const TEST_CONTENT: &[u8] = b"this is the real LFS file content";

/// Start a mock LFS server that responds to batch API + serves download.
fn start_mock_lfs_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let download_url = format!("http://{}/download/{}", addr, TEST_OID);

    let handle = std::thread::spawn(move || {
        // Handle 2 requests: batch API + download
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);

            if request.contains("/objects/batch") {
                // Respond with batch API response
                let body = serde_json::json!({
                    "objects": [{
                        "oid": TEST_OID,
                        "size": TEST_CONTENT.len(),
                        "actions": {
                            "download": {
                                "href": download_url,
                            }
                        }
                    }]
                });
                let body_str = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_str.len(),
                    body_str
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("/download/") {
                // Serve the actual content
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    TEST_CONTENT.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(TEST_CONTENT).unwrap();
            }
        }
    });

    (url, handle)
}

#[tokio::test]
async fn lfs_batch_api_fallback() {
    let (server_url, _handle) = start_mock_lfs_server();

    // Create an empty store (no LFS objects in S3)
    let tmp = tempfile::TempDir::new().unwrap();
    let store_dir = tmp.path().join("empty_store");
    std::fs::create_dir_all(&store_dir).unwrap();

    let config = BackendConfig::Fs { root: store_dir.to_str().unwrap().into() };
    let op = afs_store::backend::create_operator(&config).unwrap();
    let cache = BlobCache::new(&CacheConfig { memory_capacity: 1024 * 1024, disk_dir: None, disk_capacity: 0 })
        .await
        .unwrap();

    let client = reqwest::Client::new();

    // Fetch should fail in S3, then fall back to batch API
    let data = afs_store::lfs::fetch_lfs_object(
        TEST_OID,
        &cache,
        &op,
        Some(&server_url),
        &client,
    )
    .await
    .unwrap();

    assert_eq!(data.as_ref(), TEST_CONTENT);

    // Should be cached now
    assert!(cache.contains(TEST_OID));
}

/// Test that batch API error responses are handled properly.
fn start_error_lfs_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).unwrap();

        let body = serde_json::json!({
            "objects": [{
                "oid": TEST_OID,
                "size": 0,
                "error": {
                    "code": 404,
                    "message": "Object not found"
                }
            }]
        });
        let body_str = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    (url, handle)
}

#[tokio::test]
async fn lfs_batch_api_object_error() {
    let (server_url, _handle) = start_error_lfs_server();

    let tmp = tempfile::TempDir::new().unwrap();
    let store_dir = tmp.path().join("empty_store");
    std::fs::create_dir_all(&store_dir).unwrap();

    let config = BackendConfig::Fs { root: store_dir.to_str().unwrap().into() };
    let op = afs_store::backend::create_operator(&config).unwrap();
    let cache = BlobCache::new(&CacheConfig { memory_capacity: 1024 * 1024, disk_dir: None, disk_capacity: 0 })
        .await
        .unwrap();

    let client = reqwest::Client::new();

    let result = afs_store::lfs::fetch_lfs_object(
        TEST_OID,
        &cache,
        &op,
        Some(&server_url),
        &client,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("404") || err.contains("not found") || err.contains("Object not found"),
        "error should mention the LFS error: {}", err);
}
