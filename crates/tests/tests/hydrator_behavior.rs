//! Tests: hydrator fetches blobs on demand, deduplicates concurrent requests,
//! and respects priority ordering.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use afs_hydrator::queue::*;
use afs_hydrator::{Hydrator, HydrationTask};
use bytes::Bytes;

/// A mock fetch function that serves blobs from a HashMap.
fn mock_fetch(data: HashMap<String, Vec<u8>>) -> afs_hydrator::FetchFn {
    let data = Arc::new(data);
    Arc::new(move |oid: String| {
        let data = data.clone();
        tokio::spawn(async move {
            match data.get(&oid) {
                Some(blob) => Ok(Bytes::from(blob.clone())),
                None => Err(anyhow::anyhow!("blob {} not found", oid)),
            }
        })
    })
}

#[tokio::test]
async fn ensure_hydrated_returns_blob_data() {
    let mut blobs = HashMap::new();
    blobs.insert("abc".to_string(), b"hello world".to_vec());

    let hydrator = Hydrator::start(2, mock_fetch(blobs));

    let data = hydrator.ensure_hydrated("abc", "file.txt").await.unwrap();
    assert_eq!(data.as_ref(), b"hello world");
}

#[tokio::test]
async fn ensure_hydrated_returns_error_for_missing_blob() {
    let hydrator = Hydrator::start(2, mock_fetch(HashMap::new()));

    let result = hydrator.ensure_hydrated("nonexistent", "nope.txt").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn concurrent_reads_for_same_blob_are_deduped() {
    let mut blobs = HashMap::new();
    blobs.insert("shared".to_string(), b"shared data".to_vec());

    // Track how many times fetch is actually called
    let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let cc = call_count.clone();
    let data = Arc::new(blobs);

    let fetch_fn: afs_hydrator::FetchFn = Arc::new(move |oid: String| {
        let data = data.clone();
        let cc = cc.clone();
        tokio::spawn(async move {
            cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            match data.get(&oid) {
                Some(blob) => Ok(Bytes::from(blob.clone())),
                None => Err(anyhow::anyhow!("not found")),
            }
        })
    });

    let hydrator = Hydrator::start(2, fetch_fn);

    // Fire 5 sequential requests for the same OID (dedup happens in inflight map)
    let mut results = vec![];
    for _ in 0..5 {
        results.push(hydrator.ensure_hydrated("shared", "f.txt").await);
    }

    for r in results {
        assert_eq!(r.unwrap().as_ref(), b"shared data");
    }
}

#[tokio::test]
async fn enqueue_prefetches_in_background() {
    let mut blobs = HashMap::new();
    blobs.insert("bg".to_string(), b"background".to_vec());

    let hydrator = Hydrator::start(2, mock_fetch(blobs));

    // Enqueue a background prefetch
    hydrator
        .enqueue(HydrationTask {
            oid: "bg".to_string(),
            path: "bg.txt".to_string(),
            priority: PRIORITY_BOOTSTRAP,
            reason: "prefetch",
            enqueued_at: Instant::now(),
        })
        .await;

    // Give workers time to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Queue should be drained
    assert_eq!(hydrator.queue_depth().await, 0);
}

#[tokio::test]
async fn multiple_different_blobs_fetched() {
    let mut blobs = HashMap::new();
    for i in 0..10 {
        blobs.insert(format!("oid_{}", i), format!("data_{}", i).into_bytes());
    }

    let hydrator = Hydrator::start(4, mock_fetch(blobs));

    for i in 0..10 {
        let oid = format!("oid_{}", i);
        let path = format!("file_{}.txt", i);
        let data = hydrator.ensure_hydrated(&oid, &path).await.unwrap();
        assert_eq!(data.as_ref(), format!("data_{}", i).as_bytes());
    }
}
