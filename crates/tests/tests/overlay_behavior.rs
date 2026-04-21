//! Tests: overlay copy-on-write, reconcile, and resolver merge behavior.
//!
//! Scenario-driven: "when a user modifies a tracked file, the overlay captures
//! the change and the resolver returns the modified version."

use std::path::PathBuf;

use afs_db::*;
use afs_resolver::{OverlayManager, Resolver};

async fn setup() -> (sqlx::SqlitePool, tempfile::TempDir) {
    let pool = schema::open_db(":memory:").await.unwrap();
    let tmpdir = tempfile::TempDir::new().unwrap();
    (pool, tmpdir)
}

fn base_tree(generation: i64) -> Vec<BaseNode> {
    vec![
        BaseNode { generation, path: ".".into(), parent: "".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation, path: "src".into(), parent: ".".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation, path: "src/main.rs".into(), parent: "src".into(), kind: NodeKind::Blob, oid: Some("aaa".into()), mode: 0o100644, size: Some(100) },
        BaseNode { generation, path: "README.md".into(), parent: ".".into(), kind: NodeKind::Blob, oid: Some("bbb".into()), mode: 0o100644, size: Some(50) },
    ]
}

// ── Copy-on-write: modifying a tracked file ─────────────────────

#[tokio::test]
async fn modify_tracked_file_goes_to_overlay() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");

    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();

    // Simulate copy-on-write: user writes to src/main.rs
    let base = nodes::get_base_node(&pool, 1, "src/main.rs").await.unwrap().unwrap();
    let original_content = b"fn main() {}";
    let ovl = overlay.ensure_copy_on_write("src/main.rs", &base, original_content).await.unwrap();

    assert_eq!(ovl.kind, OverlayKind::Modify);
    assert_eq!(ovl.source_oid.as_deref(), Some("aaa"));

    // Backing file exists with original content
    let backing = ovl.backing.unwrap();
    let content = std::fs::read(&backing).unwrap();
    assert_eq!(content, original_content);

    // Write new content
    overlay.write_file("src/main.rs", 0, b"fn main() { println!(\"hello\"); }").await.unwrap();

    // Read back
    let new_content = std::fs::read(&backing).unwrap();
    assert!(new_content.starts_with(b"fn main() { println!"));
}

#[tokio::test]
async fn copy_on_write_is_idempotent() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    let base = nodes::get_base_node(&pool, 1, "README.md").await.unwrap().unwrap();

    let first = overlay.ensure_copy_on_write("README.md", &base, b"# Hello").await.unwrap();
    let second = overlay.ensure_copy_on_write("README.md", &base, b"# Hello").await.unwrap();

    // Same backing path both times
    assert_eq!(first.backing, second.backing);
}

// ── Create and delete ───────────────────────────────────────────

#[tokio::test]
async fn create_new_file_in_overlay() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper.clone()).unwrap();

    overlay.create_file("new_file.txt", 0o100644).await.unwrap();
    overlay.write_file("new_file.txt", 0, b"new content").await.unwrap();

    // Verify via overlay get
    let entry = overlay.get("new_file.txt").await.unwrap().unwrap();
    assert_eq!(entry.kind, OverlayKind::Create);
    assert_eq!(entry.size, 11); // "new content".len()
}

#[tokio::test]
async fn delete_tracked_file_creates_whiteout() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();

    overlay.remove("README.md").await.unwrap();

    let entry = overlay.get("README.md").await.unwrap().unwrap();
    assert!(entry.is_deleted());
}

// ── Resolver: merge snapshot + overlay ──────────────────────────

#[tokio::test]
async fn resolver_returns_base_when_no_overlay() {
    let (pool, _tmpdir) = setup().await;
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let resolver = Resolver::new(pool, 1);

    let node = resolver.resolve("src/main.rs").await.unwrap().unwrap();
    assert_eq!(node.oid.as_deref(), Some("aaa"));
    assert!(!node.from_overlay);
}

#[tokio::test]
async fn resolver_prefers_overlay_over_base() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    let base = nodes::get_base_node(&pool, 1, "README.md").await.unwrap().unwrap();
    overlay.ensure_copy_on_write("README.md", &base, b"old").await.unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    let node = resolver.resolve("README.md").await.unwrap().unwrap();
    assert!(node.from_overlay);
}

#[tokio::test]
async fn resolver_hides_deleted_files() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    overlay.remove("README.md").await.unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    // Deleted file should not be visible
    assert!(resolver.resolve("README.md").await.unwrap().is_none());

    // Should not appear in directory listing either
    let children = resolver.list_dir(".").await.unwrap();
    assert!(!children.iter().any(|n| n.path == "README.md"));
}

#[tokio::test]
async fn resolver_shows_new_overlay_files_in_listing() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    overlay.create_file("new.txt", 0o100644).await.unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    let children = resolver.list_dir(".").await.unwrap();
    assert!(children.iter().any(|n| n.path == "new.txt"));
}

// ── Reconcile: cleaning stale overlay after generation switch ───

#[tokio::test]
async fn reconcile_removes_committed_creates() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");

    // Gen 1: no new.txt
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    overlay.create_file("new.txt", 0o100644).await.unwrap();

    // Gen 2: new.txt exists in base (user committed it)
    let mut gen2 = base_tree(2);
    gen2.push(BaseNode {
        generation: 2, path: "new.txt".into(), parent: ".".into(),
        kind: NodeKind::Blob, oid: Some("ddd".into()), mode: 0o100644, size: Some(10),
    });
    nodes::publish_generation(&pool, 2, &gen2).await.unwrap();

    // Reconcile: create entry should be removed because base now has the path
    let removed = overlay.reconcile(|path| {
        gen2.iter().find(|n| n.path == path).cloned()
    }).await.unwrap();

    assert!(removed > 0);
    assert!(overlay.get("new.txt").await.unwrap().is_none());
}

#[tokio::test]
async fn reconcile_keeps_modify_when_base_unchanged() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    let base = nodes::get_base_node(&pool, 1, "src/main.rs").await.unwrap().unwrap();
    overlay.ensure_copy_on_write("src/main.rs", &base, b"content").await.unwrap();

    // Gen 2 with same OID for src/main.rs
    let gen2 = base_tree(2);
    nodes::publish_generation(&pool, 2, &gen2).await.unwrap();

    let removed = overlay.reconcile(|path| {
        gen2.iter().find(|n| n.path == path).cloned()
    }).await.unwrap();

    // source_oid matches, so modify should be kept
    assert_eq!(removed, 0);
    assert!(overlay.get("src/main.rs").await.unwrap().is_some());
}

#[tokio::test]
async fn reconcile_removes_stale_modify_when_base_changed() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    let base = nodes::get_base_node(&pool, 1, "src/main.rs").await.unwrap().unwrap();
    overlay.ensure_copy_on_write("src/main.rs", &base, b"old content").await.unwrap();

    // Gen 2: src/main.rs has different OID (someone else changed it)
    let mut gen2 = base_tree(2);
    for node in &mut gen2 {
        if node.path == "src/main.rs" {
            node.oid = Some("new_oid".into());
        }
    }
    nodes::publish_generation(&pool, 2, &gen2).await.unwrap();

    let removed = overlay.reconcile(|path| {
        gen2.iter().find(|n| n.path == path).cloned()
    }).await.unwrap();

    // source_oid mismatch: overlay is stale
    assert!(removed > 0);
    assert!(overlay.get("src/main.rs").await.unwrap().is_none());
}

#[tokio::test]
async fn reconcile_removes_orphan_whiteout() {
    let (pool, tmpdir) = setup().await;
    let upper = tmpdir.path().join("upper");
    nodes::publish_generation(&pool, 1, &base_tree(1)).await.unwrap();

    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();
    overlay.remove("README.md").await.unwrap();

    // Gen 2: README.md no longer in base (someone else deleted it too)
    let gen2: Vec<BaseNode> = base_tree(2).into_iter()
        .filter(|n| n.path != "README.md")
        .collect();
    nodes::publish_generation(&pool, 2, &gen2).await.unwrap();

    let removed = overlay.reconcile(|path| {
        gen2.iter().find(|n| n.path == path).cloned()
    }).await.unwrap();

    // Whiteout is orphan (base doesn't have the file), should be removed
    assert!(removed > 0);
}
