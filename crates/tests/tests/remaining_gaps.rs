//! Additional targeted tests for the last remaining coverage gaps.

use afs_db::*;
use afs_resolver::{OverlayManager, Resolver};

// ── overlay: rename, mkdir, list_by_prefix ──────────────────────

#[tokio::test]
async fn overlay_rename_moves_file() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();

    overlay.create_file("old.txt", 0o644).await.unwrap();
    overlay.write_file("old.txt", 0, b"content").await.unwrap();

    overlay.rename("old.txt", "new.txt").await.unwrap();

    // Old path should be deleted
    let old = overlay.get("old.txt").await.unwrap().unwrap();
    assert!(old.is_deleted());

    // New path should have the content
    let new_entry = overlay.get("new.txt").await.unwrap().unwrap();
    assert_eq!(new_entry.kind, OverlayKind::Rename);

    // Read from new backing
    let data = overlay.read_file(new_entry.backing.as_ref().unwrap(), 0, 1024).unwrap();
    assert_eq!(data, b"content");
}

#[tokio::test]
async fn overlay_mkdir_creates_directory() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();

    overlay.mkdir("newdir", 0o755).await.unwrap();

    let entry = overlay.get("newdir").await.unwrap().unwrap();
    assert_eq!(entry.kind, OverlayKind::Mkdir);
    assert!(!entry.is_deleted());
}

#[tokio::test]
async fn overlay_list_by_prefix_returns_children() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();

    overlay.create_file("src/a.rs", 0o644).await.unwrap();
    overlay.create_file("src/b.rs", 0o644).await.unwrap();
    overlay.create_file("other.txt", 0o644).await.unwrap();

    let entries = overlay.list_by_prefix("src").await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.path == "src/a.rs"));
    assert!(entries.iter().any(|e| e.path == "src/b.rs"));
}

#[tokio::test]
async fn overlay_list_root_prefix() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();

    overlay.create_file("a.txt", 0o644).await.unwrap();
    overlay.create_file("b.txt", 0o644).await.unwrap();

    // Root prefix "." lists everything
    let entries = overlay.list_by_prefix(".").await.unwrap();
    assert!(entries.len() >= 2);
}

// ── resolver/merged: overlay mkdir merges with base dir ─────────

#[tokio::test]
async fn resolver_merges_overlay_mkdir_with_base_dir() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();

    let tree = vec![
        BaseNode { generation: 1, path: ".".into(), parent: "".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation: 1, path: "existing".into(), parent: ".".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
    ];
    nodes::publish_generation(&pool, 1, &tree).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();
    // Overlay creates same dir (e.g. after git checkout creates it)
    overlay.mkdir("existing", 0o755).await.unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    // Resolve should return dir (from overlay since it's newer)
    let node = resolver.resolve("existing").await.unwrap().unwrap();
    assert!(node.is_dir());
    assert!(node.from_overlay);
}

#[tokio::test]
async fn resolver_overlay_modifies_existing_file_in_listing() {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmp = tempfile::TempDir::new().unwrap();

    let tree = vec![
        BaseNode { generation: 1, path: ".".into(), parent: "".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation: 1, path: "file.txt".into(), parent: ".".into(), kind: NodeKind::Blob, oid: Some("aaa".into()), mode: 0o100644, size: Some(10) },
    ];
    nodes::publish_generation(&pool, 1, &tree).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), tmp.path().join("upper")).unwrap();
    let base = nodes::get_base_node(&pool, 1, "file.txt").await.unwrap().unwrap();
    overlay.ensure_copy_on_write("file.txt", &base, b"original").await.unwrap();
    overlay.write_file("file.txt", 0, b"modified content!!").await.unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    // In directory listing, the file should show overlay size
    let children = resolver.list_dir(".").await.unwrap();
    let file = children.iter().find(|n| n.path == "file.txt").unwrap();
    assert!(file.from_overlay);
    assert_eq!(file.size, Some(18)); // "modified content!!".len()
}

// ── db/nodes.rs: the 1 uncovered line (delete_overlay_node) ─────

#[tokio::test]
async fn delete_overlay_node_removes_entry() {
    let pool = schema::open_db(":memory:").await.unwrap();

    let node = OverlayNode {
        path: "to_delete.txt".into(),
        kind: OverlayKind::Create,
        backing: None,
        mode: 0o644,
        size: 0,
        mtime_ns: 0,
        source_oid: None,
    };
    nodes::upsert_overlay_node(&pool, &node).await.unwrap();
    assert!(nodes::get_overlay_node(&pool, "to_delete.txt").await.unwrap().is_some());

    nodes::delete_overlay_node(&pool, "to_delete.txt").await.unwrap();
    assert!(nodes::get_overlay_node(&pool, "to_delete.txt").await.unwrap().is_none());
}

// ── hydrator: worker error handling paths ───────────────────────

#[tokio::test]
async fn hydrator_worker_handles_panic_in_fetch() {
    use std::sync::Arc;
    use afs_hydrator::Hydrator;

    let fetch_fn: afs_hydrator::FetchFn = Arc::new(|_oid: String| {
        tokio::spawn(async {
            panic!("simulated panic in fetch")
        })
    });

    let hydrator = Hydrator::start(1, fetch_fn);
    let result = hydrator.ensure_hydrated("panic_oid", "file.txt").await;
    assert!(result.is_err());
}

// ── fuse/inode: the 1 uncovered line (get_by_path miss after forget) ──

#[test]
fn inode_table_path_removed_after_forget() {
    use afs_fuse::inode::{InodeKind, InodeTable};

    let mut table = InodeTable::new();
    let ino = table.get_or_insert("temp.txt", InodeKind::File, 0o644);

    assert!(table.get_by_path("temp.txt").is_some());
    table.forget(ino, 1);
    assert!(table.get_by_path("temp.txt").is_none());
}

// ── store/cache: contains on cached item ────────────────────────

#[tokio::test]
async fn cache_contains_after_insert() {
    let cache = afs_store::cache::BlobCache::new(
        &afs_store::cache::CacheConfig { memory_capacity: 1024 * 1024, disk_dir: None, disk_capacity: 0 }
    ).await.unwrap();

    assert!(!cache.contains("oid1"));
    cache.insert("oid1".into(), bytes::Bytes::from_static(b"data"));
    assert!(cache.contains("oid1"));
}

// ── store/pack: unsupported version ─────────────────────────────

#[test]
fn pack_unsupported_version() {
    let mut data = Vec::new();
    data.extend_from_slice(b"AFPK");
    data.extend_from_slice(&99u32.to_le_bytes()); // version 99
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&[0u8; 32]); // footer
    assert!(afs_store::pack::read_pack_header(&data).is_err());
}
