//! Benchmark: BlobStore performance with FS backend vs S3 (MinIO) backend.
//!
//! Prerequisites:
//!   docker compose -f docker-compose.bench.yml up -d
//!   # wait for minio healthy, then create bucket:
//!   docker compose -f docker-compose.bench.yml exec minio mc alias set local http://localhost:9000 minioadmin minioadmin
//!   docker compose -f docker-compose.bench.yml exec minio mc mb local/afs-bench
//!
//! Run:
//!   cargo bench -p afs-tests --bench store_backend

use std::sync::Arc;

use anyhow::Result;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;

use afs_store::backend::BackendConfig;
use afs_store::cache::CacheConfig;
use afs_store::BlobStore;

/// Generate random blob data and return (oid, data).
/// OID is truncated to 40 hex chars (SHA-1 length) for pack format compatibility.
fn random_blob(size: usize) -> (String, Vec<u8>) {
    let mut data = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut data);
    let hash = Sha256::digest(&data);
    let oid: String = hash.iter().map(|b| format!("{:02x}", b)).collect::<String>();
    let oid = oid[..40].to_string();
    (oid, data)
}

/// Generate N pre-made blobs for benchmarking (avoids measuring RNG/hash in hot path).
fn pre_generate_blobs(count: usize, size: usize) -> Vec<(String, Vec<u8>)> {
    (0..count).map(|_| random_blob(size)).collect()
}

/// Create a BlobStore with the given backend config.
async fn make_store(config: BackendConfig) -> Result<BlobStore> {
    let pool = sqlx::SqlitePool::connect(":memory:").await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pack_index (
            oid TEXT NOT NULL,
            pack_id TEXT NOT NULL,
            offset INTEGER NOT NULL,
            comp_size INTEGER NOT NULL,
            raw_size INTEGER NOT NULL,
            PRIMARY KEY (oid)
        )",
    )
    .execute(&pool)
    .await?;

    let cache_config = CacheConfig {
        memory_capacity: 64 * 1024 * 1024,
        disk_dir: None,
        disk_capacity: 0,
    };
    BlobStore::new(&config, &cache_config, pool).await
}

fn fs_config(dir: &str) -> BackendConfig {
    BackendConfig::Fs {
        root: dir.to_string(),
    }
}

fn minio_config() -> BackendConfig {
    BackendConfig::S3 {
        bucket: "afs-bench".to_string(),
        region: Some("us-east-1".to_string()),
        endpoint: Some("http://localhost:9100".to_string()),
        access_key_id: Some("minioadmin".to_string()),
        secret_access_key: Some("minioadmin".to_string()),
        prefix: None,
    }
}

/// Check if MinIO is reachable.
fn minio_available() -> bool {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        reqwest::Client::new()
            .get("http://localhost:9100/minio/health/live")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .is_ok()
    })
}

fn bench_put_blob(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let fs_store = rt.block_on(make_store(fs_config(tmp.path().to_str().unwrap()))).unwrap();

    let has_minio = minio_available();
    let minio_store = if has_minio {
        Some(rt.block_on(make_store(minio_config())).unwrap())
    } else {
        eprintln!("⚠ MinIO not available at localhost:9100 — skipping S3 benchmarks");
        None
    };

    let sizes: &[usize] = &[
        1024,            // 1 KB
        64 * 1024,       // 64 KB
        256 * 1024,      // 256 KB
        1024 * 1024,     // 1 MB
        4 * 1024 * 1024, // 4 MB
    ];

    let mut group = c.benchmark_group("put_blob");

    for &size in sizes {
        group.throughput(Throughput::Bytes(size as u64));

        // Pre-generate blobs to avoid measuring RNG/hash overhead
        let blobs = pre_generate_blobs(200, size);

        group.bench_with_input(BenchmarkId::new("fs", size), &size, |b, _| {
            let blobs = &blobs;
            let mut idx = 0usize;
            b.to_async(&rt).iter(|| {
                let (ref oid, ref data) = blobs[idx % blobs.len()];
                idx += 1;
                let store = &fs_store;
                async move {
                    store.put_blob(oid, data).await.unwrap();
                }
            });
        });

        if let Some(ref s3_store) = minio_store {
            group.bench_with_input(BenchmarkId::new("s3_minio", size), &size, |b, _| {
                let blobs = &blobs;
                let mut idx = 0usize;
                b.to_async(&rt).iter(|| {
                    let (ref oid, ref data) = blobs[idx % blobs.len()];
                    idx += 1;
                    let store = s3_store;
                    async move {
                        store.put_blob(oid, data).await.unwrap();
                    }
                });
            });
        }
    }

    group.finish();
}

/// Baseline: raw std::fs::write to measure opendal overhead.
fn bench_put_blob_raw_fs(c: &mut Criterion) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();

    let sizes: &[usize] = &[1024, 64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024];

    let mut group = c.benchmark_group("put_blob_raw");

    for &size in sizes {
        group.throughput(Throughput::Bytes(size as u64));

        let blobs = pre_generate_blobs(200, size);

        group.bench_with_input(BenchmarkId::new("std_fs_write", size), &size, |b, _| {
            let blobs = &blobs;
            let mut idx = 0usize;
            b.iter(|| {
                let (ref oid, ref data) = blobs[idx % blobs.len()];
                idx += 1;
                let dir = base.join(&oid[..2]);
                std::fs::create_dir_all(&dir).ok();
                std::fs::write(dir.join(oid), data).unwrap();
            });
        });
    }

    group.finish();
}

fn bench_get_blob(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let fs_store = rt.block_on(make_store(fs_config(tmp.path().to_str().unwrap()))).unwrap();

    let has_minio = minio_available();
    let minio_store = if has_minio {
        Some(rt.block_on(make_store(minio_config())).unwrap())
    } else {
        None
    };

    let sizes: &[usize] = &[
        1024,            // 1 KB
        64 * 1024,       // 64 KB
        256 * 1024,      // 256 KB
        1024 * 1024,     // 1 MB
        4 * 1024 * 1024, // 4 MB
    ];

    // Pre-populate blobs
    let mut blobs: Vec<(String, usize)> = Vec::new();
    for &size in sizes {
        let (oid, data) = random_blob(size);
        rt.block_on(fs_store.put_blob(&oid, &data)).unwrap();
        if let Some(ref s3_store) = minio_store {
            rt.block_on(s3_store.put_blob(&oid, &data)).unwrap();
        }
        blobs.push((oid, size));
    }

    let mut group = c.benchmark_group("get_blob");

    for (oid, size) in &blobs {
        group.throughput(Throughput::Bytes(*size as u64));

        // Warm read (from cache)
        group.bench_with_input(BenchmarkId::new("fs_cached", size), &oid, |b, oid| {
            b.to_async(&rt).iter(|| async {
                let data = fs_store.get_blob(oid).await.unwrap();
                criterion::black_box(data);
            });
        });

        if let Some(ref s3_store) = minio_store {
            group.bench_with_input(BenchmarkId::new("s3_minio_cached", size), &oid, |b, oid| {
                b.to_async(&rt).iter(|| async {
                    let data = s3_store.get_blob(oid).await.unwrap();
                    criterion::black_box(data);
                });
            });
        }
    }

    group.finish();
}

fn bench_put_pack(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let fs_store = rt.block_on(make_store(fs_config(tmp.path().to_str().unwrap()))).unwrap();

    let has_minio = minio_available();
    let minio_store = if has_minio {
        Some(rt.block_on(make_store(minio_config())).unwrap())
    } else {
        None
    };

    let pack_sizes: &[usize] = &[20, 100];

    let mut group = c.benchmark_group("put_pack");

    for &count in pack_sizes {
        let entries: Vec<afs_store::pack::PackEntryData> = (0..count)
            .map(|_| {
                let (oid, data) = random_blob(8 * 1024);
                afs_store::pack::PackEntryData { oid, data }
            })
            .collect();

        let total_bytes: usize = entries.iter().map(|e| e.data.len()).sum();
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new("fs", format!("{}x8KB", count)),
            &entries,
            |b, entries| {
                b.to_async(&rt).iter(|| async {
                    fs_store.put_pack(entries).await.unwrap();
                });
            },
        );

        if let Some(ref s3_store) = minio_store {
            group.bench_with_input(
                BenchmarkId::new("s3_minio", format!("{}x8KB", count)),
                &entries,
                |b, entries| {
                    b.to_async(&rt).iter(|| async {
                        s3_store.put_pack(entries).await.unwrap();
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_hydrator_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let fs_store = Arc::new(
        rt.block_on(make_store(fs_config(tmp.path().to_str().unwrap()))).unwrap(),
    );

    // Pre-populate 50 blobs of 64KB
    let mut oids: Vec<String> = Vec::new();
    for _ in 0..50 {
        let (oid, data) = random_blob(64 * 1024);
        rt.block_on(fs_store.put_blob(&oid, &data)).unwrap();
        oids.push(oid);
    }

    let mut group = c.benchmark_group("hydrator");
    group.throughput(Throughput::Elements(50));

    for workers in [2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("ensure_hydrated_50blobs", format!("{}workers", workers)),
            &workers,
            |b, &w| {
                b.to_async(&rt).iter(|| {
                    let store = fs_store.clone();
                    let oids = oids.clone();
                    async move {
                        let fetch_fn: afs_hydrator::FetchFn = Arc::new(move |oid: String| {
                            let store = store.clone();
                            tokio::spawn(async move {
                                let data = store.get_blob(&oid).await?;
                                Ok(data)
                            })
                        });
                        let hydrator = afs_hydrator::Hydrator::start(w, fetch_fn);

                        let handles: Vec<_> = oids
                            .iter()
                            .map(|oid| {
                                let h = &hydrator;
                                h.ensure_hydrated(oid, "bench")
                            })
                            .collect();

                        for h in handles {
                            h.await.unwrap();
                        }
                    }
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: upload N blobs concurrently vs serially (measures optimization #1).
fn bench_concurrent_put(c: &mut Criterion) {
    let has_minio = minio_available();
    if !has_minio {
        eprintln!("⚠ MinIO not available — skipping concurrent put benchmarks");
        return;
    }

    let rt = Runtime::new().unwrap();
    let store = Arc::new(rt.block_on(make_store(minio_config())).unwrap());

    let blob_count = 50;
    let blob_size = 64 * 1024; // 64KB each
    let blobs = pre_generate_blobs(blob_count, blob_size);

    let mut group = c.benchmark_group("concurrent_put");
    group.throughput(Throughput::Elements(blob_count as u64));
    group.sample_size(20);

    // Serial: one at a time
    group.bench_function("s3_serial_50x64KB", |b| {
        let store = store.clone();
        let blobs = &blobs;
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            async move {
                for (oid, data) in blobs {
                    store.put_blob(oid, data).await.unwrap();
                }
            }
        });
    });

    // Concurrent with buffer_unordered(8)
    group.bench_function("s3_concurrent8_50x64KB", |b| {
        let store = store.clone();
        let blobs = &blobs;
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            async move {
                use futures::stream::{self, StreamExt};
                stream::iter(blobs.iter())
                    .map(|(oid, data)| {
                        let store = store.clone();
                        async move { store.put_blob(oid, data).await.unwrap() }
                    })
                    .buffer_unordered(8)
                    .collect::<Vec<_>>()
                    .await;
            }
        });
    });

    group.finish();
}

/// Benchmark: cold S3 reads (cache miss) — serial vs concurrent (measures real S3 latency).
fn bench_cold_get(c: &mut Criterion) {
    let has_minio = minio_available();
    if !has_minio {
        return;
    }

    let rt = Runtime::new().unwrap();

    let blob_count = 30;
    let blob_size = 64 * 1024;
    let blobs = pre_generate_blobs(blob_count, blob_size);

    // Upload blobs to MinIO first
    let upload_store = rt.block_on(make_store(minio_config())).unwrap();
    for (oid, data) in &blobs {
        rt.block_on(upload_store.put_blob(oid, data)).unwrap();
    }
    drop(upload_store);

    let oids: Vec<&str> = blobs.iter().map(|(oid, _)| oid.as_str()).collect();

    let mut group = c.benchmark_group("cold_get");
    group.throughput(Throughput::Elements(blob_count as u64));
    group.sample_size(20);

    // Serial cold reads — fresh store each iteration (empty cache)
    group.bench_function("s3_serial_30x64KB", |b| {
        b.to_async(&rt).iter(|| {
            let oids = oids.clone();
            async move {
                let store = make_store(minio_config()).await.unwrap();
                for oid in &oids {
                    store.get_blob(oid).await.unwrap();
                }
            }
        });
    });

    // Concurrent cold reads
    group.bench_function("s3_concurrent8_30x64KB", |b| {
        b.to_async(&rt).iter(|| {
            let oids = oids.clone();
            async move {
                let store = Arc::new(make_store(minio_config()).await.unwrap());
                use futures::stream::{self, StreamExt};
                stream::iter(oids.iter())
                    .map(|oid| {
                        let store = store.clone();
                        let oid = oid.to_string();
                        async move { store.get_blob(&oid).await.unwrap() }
                    })
                    .buffer_unordered(8)
                    .collect::<Vec<_>>()
                    .await;
            }
        });
    });

    group.finish();
}

/// Benchmark: get_blobs_batch vs individual get_blob (measures optimization #4).
fn bench_batch_get(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let store = rt.block_on(make_store(fs_config(tmp.path().to_str().unwrap()))).unwrap();

    // Pre-populate blobs and packs so pack_index has entries
    let blob_count = 100;
    let blob_size = 8 * 1024;
    let blobs = pre_generate_blobs(blob_count, blob_size);

    // Insert via put_pack so they're in pack_index (not just cache)
    let entries: Vec<afs_store::pack::PackEntryData> = blobs
        .iter()
        .map(|(oid, data)| afs_store::pack::PackEntryData {
            oid: oid.clone(),
            data: data.clone(),
        })
        .collect();
    rt.block_on(store.put_pack(&entries)).unwrap();

    let oids: Vec<String> = blobs.iter().map(|(oid, _)| oid.clone()).collect();

    let mut group = c.benchmark_group("batch_get");
    group.throughput(Throughput::Elements(blob_count as u64));

    // Individual get_blob (N queries)
    group.bench_function("individual_100x8KB", |b| {
        let store = &store;
        let oids = &oids;
        b.to_async(&rt).iter(|| async move {
            for oid in oids {
                store.get_blob(oid).await.unwrap();
            }
        });
    });

    // Batched get_blobs_batch (1 query)
    group.bench_function("batched_100x8KB", |b| {
        let store = &store;
        let oids = &oids;
        b.to_async(&rt).iter(|| async move {
            let refs: Vec<&str> = oids.iter().map(|s| s.as_str()).collect();
            store.get_blobs_batch(&refs).await.unwrap();
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_put_blob,
    bench_put_blob_raw_fs,
    bench_get_blob,
    bench_put_pack,
    bench_hydrator_throughput,
    bench_concurrent_put,
    bench_cold_get,
    bench_batch_get,
);
criterion_main!(benches);
