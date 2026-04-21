//! Tests: FUSE filesystem logic without requiring /dev/fuse.
//!
//! Tests helper functions directly and the resolver integration that
//! powers the FUSE layer. We test the resolver (which is the FUSE "brain")
//! directly because AfsFilesystem uses block_on internally.

use afs_db::*;
use afs_fuse::AfsFilesystem;
use afs_resolver::{OverlayManager, Resolver};

async fn setup_resolver(tmpdir: &tempfile::TempDir) -> (Resolver, sqlx::SqlitePool) {
    let pool = schema::open_db(":memory:").await.unwrap();

    let tree = vec![
        BaseNode { generation: 1, path: ".".into(), parent: "".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation: 1, path: "src".into(), parent: ".".into(), kind: NodeKind::Dir, oid: None, mode: 0o40755, size: None },
        BaseNode { generation: 1, path: "src/main.rs".into(), parent: "src".into(), kind: NodeKind::Blob, oid: Some("aaaa".into()), mode: 0o100644, size: Some(100) },
        BaseNode { generation: 1, path: "README.md".into(), parent: ".".into(), kind: NodeKind::Blob, oid: Some("bbbb".into()), mode: 0o100644, size: Some(50) },
        BaseNode { generation: 1, path: "link".into(), parent: ".".into(), kind: NodeKind::Symlink, oid: Some("cccc".into()), mode: 0o120000, size: None },
        BaseNode { generation: 1, path: "model.bin".into(), parent: ".".into(), kind: NodeKind::Lfs, oid: Some("dddd".repeat(16)), mode: 0o100644, size: Some(10_000_000) },
    ];

    nodes::publish_generation(&pool, 1, &tree).await.unwrap();

    let upper = tmpdir.path().join("upper");
    let overlay = OverlayManager::new(pool.clone(), upper).unwrap();

    let mut resolver = Resolver::new(pool.clone(), 1);
    resolver.set_overlay(overlay);

    (resolver, pool)
}

// ── Gitfile synthesis ───────────────────────────────────────────

#[test]
fn gitfile_content_points_to_gitdir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = tmp.path().join("gitdir");
    std::fs::create_dir_all(&gitdir).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let resolver = Resolver::new(
        rt.block_on(schema::open_db(":memory:")).unwrap(),
        1,
    );
    let fs = AfsFilesystem::new(resolver, gitdir.clone(), rt.handle().clone());

    let content = fs.gitfile_content();
    let text = String::from_utf8(content).unwrap();
    assert!(text.starts_with("gitdir: "));
    assert!(text.ends_with("\n"));
    assert!(text.contains(gitdir.to_str().unwrap()));
}

// ── Path construction ───────────────────────────────────────────

#[test]
fn child_path_from_root() {
    assert_eq!(AfsFilesystem::child_path(".", "README.md"), "README.md");
}

#[test]
fn child_path_from_subdir() {
    assert_eq!(AfsFilesystem::child_path("src", "main.rs"), "src/main.rs");
}

#[test]
fn child_path_nested() {
    assert_eq!(AfsFilesystem::child_path("a/b/c", "file.txt"), "a/b/c/file.txt");
}

// ── Resolver integration (the "brain" behind FUSE) ──────────────

#[tokio::test]
async fn resolve_regular_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    let node = resolver.resolve("README.md").await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Blob);
    assert_eq!(node.oid.as_deref(), Some("bbbb"));
    assert_eq!(node.size, Some(50));
}

#[tokio::test]
async fn resolve_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    let node = resolver.resolve("src").await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Dir);
}

#[tokio::test]
async fn resolve_lfs_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    let node = resolver.resolve("model.bin").await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Lfs);
    assert_eq!(node.size, Some(10_000_000));
}

#[tokio::test]
async fn resolve_nonexistent_returns_none() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    assert!(resolver.resolve("nope.txt").await.unwrap().is_none());
}

#[tokio::test]
async fn list_root_children() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    let children = resolver.list_dir(".").await.unwrap();
    let names: Vec<&str> = children.iter().map(|n| n.name()).collect();

    assert!(names.contains(&"src"));
    assert!(names.contains(&"README.md"));
    assert!(names.contains(&"link"));
    assert!(names.contains(&"model.bin"));
}

#[tokio::test]
async fn list_subdir_children() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    let children = resolver.list_dir("src").await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name(), "main.rs");
}

// ── Overlay interaction through resolver ────────────────────────

#[tokio::test]
async fn overlay_delete_hides_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    resolver.overlay().unwrap().remove("README.md").await.unwrap();

    assert!(resolver.resolve("README.md").await.unwrap().is_none());

    let children = resolver.list_dir(".").await.unwrap();
    assert!(!children.iter().any(|n| n.name() == "README.md"));
}

#[tokio::test]
async fn overlay_create_adds_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (resolver, _) = setup_resolver(&tmp).await;

    resolver.overlay().unwrap().create_file("new.txt", 0o100644).await.unwrap();

    let children = resolver.list_dir(".").await.unwrap();
    assert!(children.iter().any(|n| n.name() == "new.txt"));

    let node = resolver.resolve("new.txt").await.unwrap().unwrap();
    assert!(node.from_overlay);
}
