//! End-to-end FUSE tests: mount a real filesystem and operate on it.
//!
//! Requires /dev/fuse. Skipped automatically if FUSE is unavailable.
//! Run with: cargo test -p afs-tests --test fuse_e2e

use std::path::{Path, PathBuf};
use std::time::Duration;

use afs_db::*;
use afs_fuse::AfsFilesystem;
use serial_test::serial;
use afs_resolver::{OverlayManager, Resolver};

/// Check if FUSE is available; skip test if not.
fn require_fuse() {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not available");
        return;
    }
}

/// Create a temp git repo with known files, return (gitdir, repo_dir).
fn create_test_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "test@test.com"]);
    git(&repo, &["config", "user.name", "Test"]);

    std::fs::write(repo.join("README.md"), "# Hello\nThis is a test repo.\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();

    // A binary-ish file
    std::fs::write(repo.join("data.bin"), vec![0u8; 256]).unwrap();

    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);

    repo.join(".git")
}

fn git(dir: &Path, args: &[&str]) {
    let s = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(s.success(), "git {:?} failed in {:?}", args, dir);
}

/// Mount the filesystem and return the mount handle.
/// Caller must hold the handle alive to keep the mount active.
fn mount_fs(
    gitdir: &Path,
    mount_path: &Path,
    rt: &tokio::runtime::Runtime,
) -> fuser::BackgroundSession {
    std::fs::create_dir_all(mount_path).unwrap();

    // Use a file-backed DB so overlay state persists across async boundaries
    let db_path = gitdir.parent().unwrap().join("test.db");
    let pool = rt.block_on(schema::open_db(db_path.to_str().unwrap())).unwrap();
    let nodes = afs_indexer::build_tree_index(gitdir, 1).unwrap();
    rt.block_on(nodes::publish_generation(&pool, 1, &nodes)).unwrap();

    let upper_dir = gitdir.parent().unwrap().join("upper");
    let overlay = OverlayManager::new(pool.clone(), upper_dir).unwrap();

    let mut resolver = Resolver::new(pool, 1);
    resolver.set_overlay(overlay);

    let fs = AfsFilesystem::new(resolver, gitdir.to_path_buf(), rt.handle().clone());

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("afs-test".into()),
    ];

    let session = fuser::spawn_mount2(fs, mount_path, &config)
        .expect("FUSE mount failed");

    // Wait for mount to become ready
    for _ in 0..100 {
        if mount_path.join(".git").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(mount_path.join(".git").exists(), "FUSE mount did not become ready");

    session
}

// ── Read operations ─────────────────────────────────────────────

#[test]
#[serial]
fn read_file_contents() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // Read README.md
    let content = std::fs::read_to_string(mount.join("README.md")).unwrap();
    assert!(content.contains("Hello"));
    assert!(content.contains("test repo"));

    // Read src/main.rs
    let main_rs = std::fs::read_to_string(mount.join("src/main.rs")).unwrap();
    assert!(main_rs.contains("fn main()"));
    assert!(main_rs.contains("println!"));
}

#[test]
#[serial]
fn list_directory() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // List root
    let entries: Vec<String> = std::fs::read_dir(&mount)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(entries.contains(&".git".to_string()), "missing .git: {:?}", entries);
    assert!(entries.contains(&"README.md".to_string()), "missing README.md: {:?}", entries);
    assert!(entries.contains(&"src".to_string()), "missing src: {:?}", entries);
    assert!(entries.contains(&".gitignore".to_string()), "missing .gitignore: {:?}", entries);

    // List src/
    let src_entries: Vec<String> = std::fs::read_dir(mount.join("src"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(src_entries.contains(&"main.rs".to_string()));
    assert!(src_entries.contains(&"lib.rs".to_string()));
}

#[test]
#[serial]
fn stat_file_attributes() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // Regular file
    let meta = std::fs::metadata(mount.join("README.md")).unwrap();
    assert!(meta.is_file());
    assert!(meta.len() > 0);

    // Directory
    let dir_meta = std::fs::metadata(mount.join("src")).unwrap();
    assert!(dir_meta.is_dir());

    // .git gitfile
    let git_meta = std::fs::metadata(mount.join(".git")).unwrap();
    assert!(git_meta.is_file());
    let git_content = std::fs::read_to_string(mount.join(".git")).unwrap();
    assert!(git_content.starts_with("gitdir: "));
}

#[test]
#[serial]
fn nonexistent_file_returns_error() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    assert!(std::fs::metadata(mount.join("nonexistent.txt")).is_err());
    assert!(std::fs::read_to_string(mount.join("nope")).is_err());
}

#[test]
#[serial]
fn read_binary_file() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    let data = std::fs::read(mount.join("data.bin")).unwrap();
    assert_eq!(data.len(), 256);
    assert!(data.iter().all(|&b| b == 0));
}

// ── Git operations from inside the mount ────────────────────────

#[test]
#[serial]
fn git_log_works_inside_mount() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    let output = std::process::Command::new("git")
        .args(["log", "--oneline", "-1"])
        .current_dir(&mount)
        .output()
        .unwrap();

    assert!(output.status.success(), "git log failed");
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("initial"), "log output: {}", log);
}

// ── Write operations ────────────────────────────────────────────

#[test]
#[serial]
fn create_and_read_new_file() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // Create a new file
    std::fs::write(mount.join("new_file.txt"), "hello from test").unwrap();

    // Read it back
    let content = std::fs::read_to_string(mount.join("new_file.txt")).unwrap();
    assert_eq!(content, "hello from test");

    // Should appear in directory listing
    let entries: Vec<String> = std::fs::read_dir(&mount)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.contains(&"new_file.txt".to_string()));
}

#[test]
#[serial]
fn mkdir_and_create_file_inside() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    std::fs::create_dir(mount.join("newdir")).unwrap();
    assert!(mount.join("newdir").is_dir());

    std::fs::write(mount.join("newdir/file.txt"), "nested content").unwrap();
    let content = std::fs::read_to_string(mount.join("newdir/file.txt")).unwrap();
    assert_eq!(content, "nested content");
}

#[test]
#[serial]
fn delete_file() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // File exists
    assert!(mount.join("README.md").exists());

    // Delete it
    std::fs::remove_file(mount.join("README.md")).unwrap();

    // Should be gone
    assert!(!mount.join("README.md").exists());
}

#[test]
#[serial]
fn modify_existing_file() {
    require_fuse();
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_test_repo(tmp.path());
    let mount = tmp.path().join("mnt");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let _session = mount_fs(&gitdir, &mount, &rt);

    // Read original content
    let original = std::fs::read_to_string(mount.join("README.md")).unwrap();
    assert!(original.contains("Hello"));

    // Overwrite the file (triggers copy-on-write)
    std::fs::write(mount.join("README.md"), "# Modified\nNew content.\n").unwrap();

    // Read back modified content
    let modified = std::fs::read_to_string(mount.join("README.md")).unwrap();
    assert_eq!(modified, "# Modified\nNew content.\n");
    assert!(!modified.contains("Hello"));
}
