//! Tests: storing and retrieving tree snapshots and overlay entries.
//!
//! These tests verify the database layer from a functional perspective:
//! "can I store a tree and get it back?" rather than "does this SQL work?"

use afs_db::*;

async fn fresh_db() -> sqlx::SqlitePool {
    schema::open_db(":memory:").await.unwrap()
}

fn make_node(generation: i64, path: &str, kind: NodeKind, oid: Option<&str>) -> BaseNode {
    BaseNode {
        generation,
        path: path.to_string(),
        parent: parent_dir(path),
        kind,
        oid: oid.map(|s| s.to_string()),
        mode: if kind == NodeKind::Dir { 0o40755 } else { 0o100644 },
        size: if kind == NodeKind::Dir { None } else { Some(42) },
    }
}

// ── Snapshot: store a tree and retrieve it ──────────────────────

#[tokio::test]
async fn store_tree_and_list_children() {
    let pool = fresh_db().await;

    let nodes = vec![
        make_node(1, ".", NodeKind::Dir, None),
        make_node(1, "src", NodeKind::Dir, None),
        make_node(1, "src/main.rs", NodeKind::Blob, Some("aaa")),
        make_node(1, "src/lib.rs", NodeKind::Blob, Some("bbb")),
        make_node(1, "README.md", NodeKind::Blob, Some("ccc")),
    ];

    nodes::publish_generation(&pool, 1, &nodes).await.unwrap();

    // Root children
    let root_children = nodes::list_children(&pool, 1, ".").await.unwrap();
    let names: Vec<&str> = root_children.iter().map(|n| n.path.as_str()).collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"README.md"));
    assert_eq!(root_children.len(), 2);

    // src/ children
    let src_children = nodes::list_children(&pool, 1, "src").await.unwrap();
    assert_eq!(src_children.len(), 2);
    assert!(src_children.iter().any(|n| n.path == "src/main.rs"));
}

#[tokio::test]
async fn get_node_by_path() {
    let pool = fresh_db().await;

    let nodes = vec![
        make_node(1, ".", NodeKind::Dir, None),
        make_node(1, "file.txt", NodeKind::Blob, Some("abc123")),
    ];
    nodes::publish_generation(&pool, 1, &nodes).await.unwrap();

    let node = nodes::get_base_node(&pool, 1, "file.txt").await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Blob);
    assert_eq!(node.oid.as_deref(), Some("abc123"));
    assert_eq!(node.size, Some(42));

    // Non-existent path
    assert!(nodes::get_base_node(&pool, 1, "nope").await.unwrap().is_none());
    // Wrong generation
    assert!(nodes::get_base_node(&pool, 2, "file.txt").await.unwrap().is_none());
}

#[tokio::test]
async fn generation_switch_replaces_tree() {
    let pool = fresh_db().await;

    let gen1 = vec![
        make_node(1, ".", NodeKind::Dir, None),
        make_node(1, "old.txt", NodeKind::Blob, Some("old")),
    ];
    nodes::publish_generation(&pool, 1, &gen1).await.unwrap();

    let gen2 = vec![
        make_node(2, ".", NodeKind::Dir, None),
        make_node(2, "new.txt", NodeKind::Blob, Some("new")),
    ];
    nodes::publish_generation(&pool, 2, &gen2).await.unwrap();

    // Gen 1 still accessible
    assert!(nodes::get_base_node(&pool, 1, "old.txt").await.unwrap().is_some());
    // Gen 2 has different content
    assert!(nodes::get_base_node(&pool, 2, "old.txt").await.unwrap().is_none());
    assert!(nodes::get_base_node(&pool, 2, "new.txt").await.unwrap().is_some());

    // Prune old generations
    nodes::prune_old_generations(&pool, 3).await.unwrap();
    assert!(nodes::get_base_node(&pool, 1, "old.txt").await.unwrap().is_none());
}

#[tokio::test]
async fn update_blob_size_after_hydration() {
    let pool = fresh_db().await;

    let nodes = vec![
        make_node(1, ".", NodeKind::Dir, None),
        BaseNode {
            generation: 1,
            path: "big.bin".to_string(),
            parent: ".".to_string(),
            kind: NodeKind::Blob,
            oid: Some("deadbeef".to_string()),
            mode: 0o100644,
            size: None, // unknown before hydration
        },
    ];
    nodes::publish_generation(&pool, 1, &nodes).await.unwrap();

    // Size starts unknown
    let before = nodes::get_base_node(&pool, 1, "big.bin").await.unwrap().unwrap();
    assert_eq!(before.size, None);

    // Hydrator reports size
    nodes::update_blob_size(&pool, 1, "deadbeef", 1048576).await.unwrap();

    let after = nodes::get_base_node(&pool, 1, "big.bin").await.unwrap().unwrap();
    assert_eq!(after.size, Some(1048576));
}

// ── Overlay: local modifications ────────────────────────────────

#[tokio::test]
async fn overlay_create_and_retrieve() {
    let pool = fresh_db().await;

    let node = OverlayNode {
        path: "new_file.txt".to_string(),
        kind: OverlayKind::Create,
        backing: Some("/tmp/upper/new_file.txt".to_string()),
        mode: 0o100644,
        size: 5,
        mtime_ns: 1000,
        source_oid: None,
    };
    nodes::upsert_overlay_node(&pool, &node).await.unwrap();

    let got = nodes::get_overlay_node(&pool, "new_file.txt").await.unwrap().unwrap();
    assert_eq!(got.kind, OverlayKind::Create);
    assert_eq!(got.size, 5);
    assert!(!got.is_deleted());
}

#[tokio::test]
async fn overlay_delete_is_whiteout() {
    let pool = fresh_db().await;

    let node = OverlayNode {
        path: "deleted.txt".to_string(),
        kind: OverlayKind::Delete,
        backing: None,
        mode: 0,
        size: 0,
        mtime_ns: 2000,
        source_oid: None,
    };
    nodes::upsert_overlay_node(&pool, &node).await.unwrap();

    let got = nodes::get_overlay_node(&pool, "deleted.txt").await.unwrap().unwrap();
    assert!(got.is_deleted());
}

#[tokio::test]
async fn overlay_dirty_count_excludes_deletes() {
    let pool = fresh_db().await;

    // One create, one modify, one delete
    for (path, kind) in [
        ("a.txt", OverlayKind::Create),
        ("b.txt", OverlayKind::Modify),
        ("c.txt", OverlayKind::Delete),
    ] {
        let node = OverlayNode {
            path: path.to_string(),
            kind,
            backing: None,
            mode: 0o100644,
            size: 0,
            mtime_ns: 0,
            source_oid: None,
        };
        nodes::upsert_overlay_node(&pool, &node).await.unwrap();
    }

    // Dirty count should be 2 (create + modify, not delete)
    let count = nodes::overlay_dirty_count(&pool).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn overlay_upsert_updates_existing() {
    let pool = fresh_db().await;

    let node = OverlayNode {
        path: "file.txt".to_string(),
        kind: OverlayKind::Create,
        backing: Some("/tmp/v1".to_string()),
        mode: 0o100644,
        size: 10,
        mtime_ns: 1000,
        source_oid: None,
    };
    nodes::upsert_overlay_node(&pool, &node).await.unwrap();

    // Update same path with new data
    let updated = OverlayNode {
        path: "file.txt".to_string(),
        kind: OverlayKind::Modify,
        backing: Some("/tmp/v2".to_string()),
        mode: 0o100644,
        size: 20,
        mtime_ns: 2000,
        source_oid: Some("abc".to_string()),
    };
    nodes::upsert_overlay_node(&pool, &updated).await.unwrap();

    let got = nodes::get_overlay_node(&pool, "file.txt").await.unwrap().unwrap();
    assert_eq!(got.kind, OverlayKind::Modify);
    assert_eq!(got.size, 20);
    assert_eq!(got.backing.as_deref(), Some("/tmp/v2"));
}

// ── Pack index: track blob locations in S3 packs ────────────────

#[tokio::test]
async fn pack_index_roundtrip() {
    let pool = fresh_db().await;

    let entries = vec![
        packs::PackEntry {
            oid: "aaa".to_string(),
            pack_id: "pack1".to_string(),
            offset: 12,
            comp_size: 100,
            raw_size: 200,
        },
        packs::PackEntry {
            oid: "bbb".to_string(),
            pack_id: "pack1".to_string(),
            offset: 140,
            comp_size: 50,
            raw_size: 80,
        },
    ];

    packs::bulk_insert_pack_entries(&pool, &entries).await.unwrap();

    let found = packs::get_pack_entry(&pool, "aaa").await.unwrap().unwrap();
    assert_eq!(found.pack_id, "pack1");
    assert_eq!(found.offset, 12);

    assert!(packs::get_pack_entry(&pool, "zzz").await.unwrap().is_none());
}

#[tokio::test]
async fn sync_state_tracks_last_synced() {
    let pool = fresh_db().await;

    assert!(packs::get_sync_state(&pool, "last_synced_oid").await.unwrap().is_none());

    packs::set_sync_state(&pool, "last_synced_oid", "abc123").await.unwrap();
    assert_eq!(
        packs::get_sync_state(&pool, "last_synced_oid").await.unwrap().as_deref(),
        Some("abc123")
    );

    // Update
    packs::set_sync_state(&pool, "last_synced_oid", "def456").await.unwrap();
    assert_eq!(
        packs::get_sync_state(&pool, "last_synced_oid").await.unwrap().as_deref(),
        Some("def456")
    );
}
