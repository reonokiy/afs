//! End-to-end tests for git-remote-afs.
//!
//! These tests build the binary and run real git commands against it
//! using a local FS backend.

use std::path::{Path, PathBuf};
use std::process::Command;

fn binary_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove "deps"
    path.push("git-remote-afs");
    path
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    // Put our binary on PATH
    let bin_dir = binary_path().parent().unwrap().to_path_buf();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", path);
    cmd
}

fn git_run(dir: &Path, args: &[&str]) -> String {
    let output = git(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {} failed to execute: {}", args.join(" "), e));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "git {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            stdout,
            stderr
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn _git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

fn gc_run(remote_store: &Path) -> String {
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(["gc", &format!("afs://{}", remote_store.display())])
        .output()
        .expect("gc failed to execute");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("gc failed:\n{}", stderr);
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct TestEnv {
    _tmp: tempfile::TempDir,
    remote_store: PathBuf,
    repo1: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let remote_store = tmp.path().join("remote");
        std::fs::create_dir_all(&remote_store).unwrap();
        let repo1 = tmp.path().join("repo1");

        // Init repo1
        git_run(tmp.path(), &["init", "-b", "main", "repo1"]);
        git_run(&repo1, &["config", "user.email", "test@test.com"]);
        git_run(&repo1, &["config", "user.name", "Test"]);

        let remote_url = format!("afs://{}", remote_store.display());
        git_run(&repo1, &["remote", "add", "origin", &remote_url]);

        Self {
            _tmp: tmp,
            remote_store,
            repo1,
        }
    }

    fn clone_to(&self, name: &str) -> PathBuf {
        let dir = self._tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git_run(self._tmp.path(), &["init", "-b", "main", name]);
        git_run(&dir, &["config", "user.email", "test@test.com"]);
        git_run(&dir, &["config", "user.name", "Test"]);
        let remote_url = format!("afs://{}", self.remote_store.display());
        git_run(&dir, &["remote", "add", "origin", &remote_url]);
        git_run(&dir, &["fetch", "origin"]);
        git_run(&dir, &["checkout", "-b", "main", "origin/main"]);
        dir
    }

    fn write_file(&self, repo: &Path, name: &str, content: &str) {
        let path = repo.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read_file(&self, repo: &Path, name: &str) -> String {
        std::fs::read_to_string(repo.join(name)).unwrap()
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[test]
fn push_and_fetch_single_commit() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "README.md", "hello afs\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "init"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Verify refs.json exists
    let refs_path = env.remote_store.join("refs.json");
    assert!(refs_path.exists(), "refs.json should exist after push");

    // Fetch into a new repo
    let repo2 = env.clone_to("repo2");
    assert_eq!(env.read_file(&repo2, "README.md"), "hello afs\n");
}

#[test]
fn push_multiple_commits() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "a.txt", "first\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "first"]);

    env.write_file(&env.repo1, "b.txt", "second\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "second"]);

    env.write_file(&env.repo1, "c.txt", "third\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "third"]);

    git_run(&env.repo1, &["push", "origin", "main"]);

    let repo2 = env.clone_to("repo2");
    let log = git_run(&repo2, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 3, "should have 3 commits: {}", log);
    assert_eq!(env.read_file(&repo2, "a.txt"), "first\n");
    assert_eq!(env.read_file(&repo2, "b.txt"), "second\n");
    assert_eq!(env.read_file(&repo2, "c.txt"), "third\n");
}

#[test]
fn incremental_push() {
    let env = TestEnv::new();

    // First push
    env.write_file(&env.repo1, "a.txt", "v1\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v1"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Second push (incremental)
    env.write_file(&env.repo1, "b.txt", "v2\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v2"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    let repo2 = env.clone_to("repo2");
    let log = git_run(&repo2, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 2);
    assert_eq!(env.read_file(&repo2, "a.txt"), "v1\n");
    assert_eq!(env.read_file(&repo2, "b.txt"), "v2\n");
}

#[test]
fn push_and_fetch_branches() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "main.txt", "on main\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "main commit"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Create and push a feature branch
    git_run(&env.repo1, &["checkout", "-b", "feature"]);
    env.write_file(&env.repo1, "feature.txt", "on feature\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "feature commit"]);
    git_run(&env.repo1, &["push", "origin", "feature"]);

    // Fetch both branches
    let repo2 = env.clone_to("repo2");
    assert!(env.read_file(&repo2, "main.txt") == "on main\n");

    git_run(&repo2, &["checkout", "-b", "feature", "origin/feature"]);
    assert_eq!(env.read_file(&repo2, "feature.txt"), "on feature\n");
    assert_eq!(env.read_file(&repo2, "main.txt"), "on main\n");
}

#[test]
fn push_delete_ref() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "a.txt", "content\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "init"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Create and push a branch
    git_run(&env.repo1, &["checkout", "-b", "to-delete"]);
    env.write_file(&env.repo1, "b.txt", "temp\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "temp"]);
    git_run(&env.repo1, &["push", "origin", "to-delete"]);

    // Verify branch exists
    let ls = git_run(&env.repo1, &["ls-remote", "origin"]);
    assert!(ls.contains("refs/heads/to-delete"), "branch should exist: {}", ls);

    // Delete remote branch
    git_run(&env.repo1, &["push", "origin", "--delete", "to-delete"]);

    // Verify branch is gone
    let ls = git_run(&env.repo1, &["ls-remote", "origin"]);
    assert!(!ls.contains("to-delete"), "branch should be deleted: {}", ls);
}

#[test]
fn ls_remote() {
    let env = TestEnv::new();

    // Empty remote
    let ls = git_run(&env.repo1, &["ls-remote", "origin"]);
    assert!(ls.is_empty(), "empty remote should have no refs");

    env.write_file(&env.repo1, "a.txt", "x\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "init"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    let ls = git_run(&env.repo1, &["ls-remote", "origin"]);
    assert!(ls.contains("refs/heads/main"), "should list main: {}", ls);
}

#[test]
fn push_binary_files() {
    let env = TestEnv::new();

    // Write a binary file (random bytes)
    let binary_data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    std::fs::write(env.repo1.join("data.bin"), &binary_data).unwrap();

    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "add binary"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    let repo2 = env.clone_to("repo2");
    let fetched = std::fs::read(repo2.join("data.bin")).unwrap();
    assert_eq!(fetched, binary_data, "binary file should round-trip");
}

#[test]
fn push_subdirectories() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "src/main.rs", "fn main() {}\n");
    env.write_file(&env.repo1, "src/lib/mod.rs", "pub mod lib;\n");
    env.write_file(&env.repo1, "docs/README.md", "# Docs\n");

    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "tree structure"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    let repo2 = env.clone_to("repo2");
    assert_eq!(env.read_file(&repo2, "src/main.rs"), "fn main() {}\n");
    assert_eq!(env.read_file(&repo2, "src/lib/mod.rs"), "pub mod lib;\n");
    assert_eq!(env.read_file(&repo2, "docs/README.md"), "# Docs\n");
}

#[test]
fn two_repos_collaborate() {
    let env = TestEnv::new();

    // Repo1 pushes initial commit
    env.write_file(&env.repo1, "shared.txt", "line1\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "repo1: init"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Repo2 clones and adds a commit
    let repo2 = env.clone_to("repo2");
    env.write_file(&repo2, "shared.txt", "line1\nline2\n");
    git_run(&repo2, &["add", "."]);
    git_run(&repo2, &["commit", "-m", "repo2: add line2"]);
    git_run(&repo2, &["push", "origin", "main"]);

    // Repo1 pulls the change
    git_run(&env.repo1, &["fetch", "origin"]);
    git_run(&env.repo1, &["merge", "origin/main", "--ff-only"]);
    assert_eq!(env.read_file(&env.repo1, "shared.txt"), "line1\nline2\n");

    // Repo1 adds another commit
    env.write_file(&env.repo1, "shared.txt", "line1\nline2\nline3\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "repo1: add line3"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Repo2 pulls
    git_run(&repo2, &["fetch", "origin"]);
    git_run(&repo2, &["merge", "origin/main", "--ff-only"]);
    assert_eq!(env.read_file(&repo2, "shared.txt"), "line1\nline2\nline3\n");

    let log = git_run(&repo2, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 3);
}

#[test]
fn force_push() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "a.txt", "v1\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v1"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    env.write_file(&env.repo1, "a.txt", "v2\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v2"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Reset back and force push
    git_run(&env.repo1, &["reset", "--hard", "HEAD~1"]);
    env.write_file(&env.repo1, "a.txt", "v3-rewritten\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v3-rewritten"]);
    git_run(&env.repo1, &["push", "--force", "origin", "main"]);

    let repo2 = env.clone_to("repo2");
    assert_eq!(env.read_file(&repo2, "a.txt"), "v3-rewritten\n");
    let log = git_run(&repo2, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 2, "should have 2 commits after rewrite: {}", log);
}

#[test]
fn gc_repacks_multiple_packs() {
    let env = TestEnv::new();

    // Push three separate times to create 3 pack files
    env.write_file(&env.repo1, "a.txt", "v1\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v1"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    env.write_file(&env.repo1, "b.txt", "v2\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v2"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    env.write_file(&env.repo1, "c.txt", "v3\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "v3"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Count packs before GC
    let pack_count_before = std::fs::read_dir(env.remote_store.join("git"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .map(|x| x == "pack")
                .unwrap_or(false)
        })
        .count();
    assert!(pack_count_before >= 3, "expected >=3 packs, got {}", pack_count_before);

    // Run GC
    let output = gc_run(&env.remote_store);
    assert!(output.contains("→ 1 pack"), "GC output: {}", output);

    // Count packs after GC
    let pack_count_after = std::fs::read_dir(env.remote_store.join("git"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .map(|x| x == "pack")
                .unwrap_or(false)
        })
        .count();
    assert_eq!(pack_count_after, 1, "should have exactly 1 pack after GC");

    // Verify data still accessible after GC
    let repo2 = env.clone_to("repo2");
    assert_eq!(env.read_file(&repo2, "a.txt"), "v1\n");
    assert_eq!(env.read_file(&repo2, "b.txt"), "v2\n");
    assert_eq!(env.read_file(&repo2, "c.txt"), "v3\n");
    let log = git_run(&repo2, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 3);
}

#[test]
fn gc_noop_with_single_pack() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "a.txt", "content\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "init"]);
    git_run(&env.repo1, &["push", "origin", "main"]);

    let output = gc_run(&env.remote_store);
    assert!(output.contains("1 packs → 1 pack") || output.contains("nothing to repack"),
        "GC should be a noop: {}", output);
}

#[test]
fn shallow_clone() {
    let env = TestEnv::new();

    // Create 5 commits
    for i in 1..=5 {
        env.write_file(&env.repo1, "file.txt", &format!("v{}\n", i));
        git_run(&env.repo1, &["add", "."]);
        git_run(&env.repo1, &["commit", "-m", &format!("commit {}", i)]);
    }
    git_run(&env.repo1, &["push", "origin", "main"]);

    // Shallow clone with depth=2
    let repo2 = env._tmp.path().join("shallow");
    git_run(env._tmp.path(), &["init", "-b", "main", "shallow"]);
    git_run(&repo2, &["config", "user.email", "test@test.com"]);
    git_run(&repo2, &["config", "user.name", "Test"]);
    let remote_url = format!("afs://{}", env.remote_store.display());
    git_run(&repo2, &["remote", "add", "origin", &remote_url]);
    git_run(&repo2, &["fetch", "--depth", "2", "origin"]);
    git_run(&repo2, &["checkout", "-b", "main", "origin/main"]);

    assert_eq!(env.read_file(&repo2, "file.txt"), "v5\n");

    // Should have a shallow file
    let shallow_file = repo2.join(".git/shallow");
    assert!(shallow_file.exists(), "shallow file should exist after --depth fetch");
}

#[test]
fn tags_push_and_fetch() {
    let env = TestEnv::new();

    env.write_file(&env.repo1, "a.txt", "tagged\n");
    git_run(&env.repo1, &["add", "."]);
    git_run(&env.repo1, &["commit", "-m", "release"]);
    git_run(&env.repo1, &["tag", "v1.0.0"]);
    git_run(&env.repo1, &["push", "origin", "main"]);
    git_run(&env.repo1, &["push", "origin", "v1.0.0"]);

    // ls-remote should show the tag
    let ls = git_run(&env.repo1, &["ls-remote", "--tags", "origin"]);
    assert!(ls.contains("refs/tags/v1.0.0"), "tag should be listed: {}", ls);

    // Fetch tag in new repo
    let repo2 = env.clone_to("repo2");
    git_run(&repo2, &["fetch", "origin", "tag", "v1.0.0"]);

    let tag_oid = git_run(&repo2, &["rev-parse", "v1.0.0"]);
    assert!(!tag_oid.is_empty(), "tag should resolve");
}
