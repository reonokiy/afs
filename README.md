# afs

FUSE filesystem that mounts git repos with S3-backed blob storage. Files are fetched lazily from S3/GCS/Azure on first access, with in-memory caching and background prefetch.

## Usage

```bash
# Start the daemon (watches for repos, auto-mounts)
afs daemon

# Clone a repo (blobless) and register it
afs clone https://github.com/user/repo /mnt/repo --branch main

# Check repo status
afs status

# Push local changes to S3
afs push myrepo --backend-config backend.toml
```

### Backend config (`backend.toml`)

```toml
type = "s3"
bucket = "my-bucket"
region = "us-east-1"
endpoint = "https://s3.amazonaws.com"
access_key_id = "..."
secret_access_key = "..."
```

### Pack config (`config.toml`)

```toml
[pack]
pack_threshold = 262144    # 256KB, blobs smaller than this are packed together
target_pack_size = 67108864  # 64MB per pack file
```

## Architecture

```
crates/
  afs/        CLI + daemon
  fuse/       FUSE filesystem (read/write/mkdir/rename/symlink)
  resolver/   Merge base snapshot + overlay (copy-on-write)
  hydrator/   Priority queue + worker pool for lazy blob fetch
  store/      BlobStore: cache -> pack_index -> S3 (multipart, concurrent)
  db/         SQLite (WAL) for tree snapshots + pack index
  indexer/    Blobless clone + tree indexing
  tests/      Integration tests + criterion benchmarks
```

## Benchmarks

Measured with local MinIO (localhost S3). Run with:

```bash
docker compose -f docker-compose.bench.yml up -d
docker compose -f docker-compose.bench.yml exec minio \
  mc alias set local http://localhost:9000 minioadmin minioadmin
docker compose -f docker-compose.bench.yml exec minio mc mb local/afs-bench
cargo bench -p afs-tests --bench store_backend
docker compose -f docker-compose.bench.yml down
```

### Single blob write (put_blob)

| Size | FS backend | S3 (MinIO) |
|------|-----------|------------|
| 1 KB | 430 us | 8.6 ms |
| 64 KB | 550 us / 111 MB/s | 10.7 ms / 5.9 MB/s |
| 256 KB | 840 us / 296 MB/s | 15.8 ms / 15.8 MB/s |
| 1 MB | 2.0 ms / 493 MB/s | 20.8 ms / 48 MB/s |
| 4 MB | 6.9 ms / 578 MB/s | 45 ms / 89 MB/s |

### Concurrent S3 uploads (the optimization that matters)

| Mode | 50x64KB total | Throughput |
|------|--------------|------------|
| Serial | 557 ms | 90 elem/s |
| Concurrent x8 | 112 ms | 448 elem/s |
| **Speedup** | **5.0x** | |

### Cold S3 reads (cache miss)

| Mode | 30x64KB total | Throughput |
|------|--------------|------------|
| Serial | 57 ms | 523 elem/s |
| Concurrent x8 | 16 ms | 1,830 elem/s |
| **Speedup** | **3.5x** | |

### Cached reads

~390 ns per blob regardless of size (foyer in-memory cache).

## License

Apache-2.0
