//! Tests: tree indexing from a real git repository.
//!
//! Creates a temporary git repo with known content, then verifies
//! the indexer produces correct base_nodes entries.

use afs_db::NodeKind;

/// Create a temporary git repo with some files, return the gitdir path.
fn create_test_repo(dir: &std::path::Path) -> std::path::PathBuf {
    let repo_dir = dir.join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();

    run_git(&repo_dir, &["init"]);
    run_git(&repo_dir, &["config", "user.email", "test@test.com"]);
    run_git(&repo_dir, &["config", "user.name", "Test"]);

    // Create files
    std::fs::write(repo_dir.join("README.md"), "# Test repo\n").unwrap();
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::write(repo_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repo_dir.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    // Create an LFS pointer file
    let lfs_pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\nsize 1048576\n";
    std::fs::write(repo_dir.join("large_asset.bin"), lfs_pointer).unwrap();

    // Create a symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink("src/main.rs", repo_dir.join("link_to_main")).unwrap();

    run_git(&repo_dir, &["add", "."]);
    run_git(&repo_dir, &["commit", "-m", "initial"]);

    repo_dir.join(".git")
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

#[test]
fn index_tree_finds_all_files() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    // Should have root, src/, README.md, src/main.rs, src/lib.rs, large_asset.bin, link_to_main
    let paths: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();

    assert!(paths.contains(&"."), "missing root");
    assert!(paths.contains(&"src"), "missing src/");
    assert!(paths.contains(&"README.md"), "missing README.md");
    assert!(paths.contains(&"src/main.rs"), "missing src/main.rs");
    assert!(paths.contains(&"src/lib.rs"), "missing src/lib.rs");
    assert!(paths.contains(&"large_asset.bin"), "missing large_asset.bin");
}

#[test]
fn index_tree_detects_directories() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    let root = nodes.iter().find(|n| n.path == ".").unwrap();
    assert_eq!(root.kind, NodeKind::Dir);

    let src = nodes.iter().find(|n| n.path == "src").unwrap();
    assert_eq!(src.kind, NodeKind::Dir);
    assert_eq!(src.parent, ".");
}

#[test]
fn index_tree_blobs_have_oids() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    let readme = nodes.iter().find(|n| n.path == "README.md").unwrap();
    assert_eq!(readme.kind, NodeKind::Blob);
    assert!(readme.oid.is_some());
    assert_eq!(readme.parent, ".");
}

#[test]
fn index_tree_detects_lfs_pointers() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    let lfs_file = nodes.iter().find(|n| n.path == "large_asset.bin").unwrap();
    assert_eq!(lfs_file.kind, NodeKind::Lfs, "LFS pointer should be detected");
    assert_eq!(
        lfs_file.oid.as_deref(),
        Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"),
        "LFS OID should be the SHA-256 from the pointer"
    );
    assert_eq!(lfs_file.size, Some(1048576), "LFS size should come from the pointer");
}

#[cfg(unix)]
#[test]
fn index_tree_detects_symlinks() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    let link = nodes.iter().find(|n| n.path == "link_to_main").unwrap();
    assert_eq!(link.kind, NodeKind::Symlink);
    assert!(link.oid.is_some());
}

#[test]
fn index_tree_parent_paths_are_correct() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();

    for node in &nodes {
        if node.path == "." {
            assert_eq!(node.parent, "");
            continue;
        }
        // Every non-root node's parent should also exist in the tree
        let parent_exists = nodes.iter().any(|n| n.path == node.parent);
        assert!(parent_exists, "parent '{}' of '{}' not in tree", node.parent, node.path);
    }
}

#[test]
fn resolve_head_returns_valid_oid() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let (oid, refname) = afs_indexer::resolve_head(&gitdir).unwrap();
    assert_eq!(oid.len(), 40, "OID should be 40 hex chars");
    assert!(oid.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(refname.contains("main") || refname.contains("master"));
}

#[tokio::test]
async fn indexed_tree_stored_and_queried_correctly() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmpdir.path());

    let pool = afs_db::schema::open_db(":memory:").await.unwrap();
    let nodes = afs_indexer::build_tree_index(&gitdir, 1).unwrap();
    afs_db::nodes::publish_generation(&pool, 1, &nodes).await.unwrap();

    // Query root children
    let root_children = afs_db::nodes::list_children(&pool, 1, ".").await.unwrap();
    let names: Vec<&str> = root_children.iter().map(|n| n.path.as_str()).collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"README.md"));

    // Query src children
    let src_children = afs_db::nodes::list_children(&pool, 1, "src").await.unwrap();
    assert!(src_children.iter().any(|n| n.path == "src/main.rs"));
    assert!(src_children.iter().any(|n| n.path == "src/lib.rs"));
}
