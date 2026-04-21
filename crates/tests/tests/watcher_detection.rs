//! Tests: watcher detects HEAD and ref changes in a git repo.

use std::path::Path;

fn create_git_repo(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "t@t.com"]);
    run_git(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("f.txt"), "initial").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "init"]);
    repo.join(".git")
}

fn run_git(dir: &Path, args: &[&str]) {
    let s = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(s.success(), "git {:?} failed", args);
}

#[test]
fn init_does_not_report_change() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_git_repo(tmp.path());

    let mut w = afs_indexer::Watcher::new(&gitdir);
    w.init().unwrap();

    // First poll after init should not report a change
    let changed = w.poll().unwrap();
    assert!(!changed);
}

#[test]
fn detects_new_commit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_git_repo(tmp.path());
    let repo = gitdir.parent().unwrap();

    let mut w = afs_indexer::Watcher::new(&gitdir);
    w.init().unwrap();
    assert!(!w.poll().unwrap());

    // Make a new commit — this changes the ref file
    std::fs::write(repo.join("f.txt"), "changed").unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-m", "second"]);

    // Watcher should detect the ref change
    let changed = w.poll().unwrap();
    assert!(changed, "watcher should detect new commit");
}

#[test]
fn no_change_when_nothing_happens() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_git_repo(tmp.path());

    let mut w = afs_indexer::Watcher::new(&gitdir);
    w.init().unwrap();

    // Multiple polls with no changes
    for _ in 0..3 {
        assert!(!w.poll().unwrap());
    }
}

#[test]
fn detects_branch_switch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_git_repo(tmp.path());
    let repo = gitdir.parent().unwrap();

    let mut w = afs_indexer::Watcher::new(&gitdir);
    w.init().unwrap();
    assert!(!w.poll().unwrap());

    // Create and switch to a new branch
    run_git(repo, &["checkout", "-b", "feature"]);

    let changed = w.poll().unwrap();
    assert!(changed, "watcher should detect branch switch");
}

#[test]
fn poll_interval_is_500ms() {
    let tmp = tempfile::TempDir::new().unwrap();
    let gitdir = create_git_repo(tmp.path());

    let w = afs_indexer::Watcher::new(&gitdir);
    assert_eq!(w.poll_interval(), std::time::Duration::from_millis(500));
}
