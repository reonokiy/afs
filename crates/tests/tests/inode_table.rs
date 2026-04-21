//! Tests: inode table manages path↔inode mapping correctly.

use afs_fuse::inode::{InodeKind, InodeTable};

#[test]
fn root_inode_is_always_present() {
    let table = InodeTable::new();
    let root = table.get(1).unwrap(); // FUSE_ROOT_ID = 1
    assert_eq!(root.path, ".");
    assert_eq!(root.kind, InodeKind::Dir);
}

#[test]
fn allocate_new_inode_for_path() {
    let mut table = InodeTable::new();

    let ino = table.get_or_insert("src/main.rs", InodeKind::File, 0o644);
    assert!(ino > 1); // Not root

    let entry = table.get(ino).unwrap();
    assert_eq!(entry.path, "src/main.rs");
    assert_eq!(entry.kind, InodeKind::File);
}

#[test]
fn same_path_returns_same_inode() {
    let mut table = InodeTable::new();

    let ino1 = table.get_or_insert("README.md", InodeKind::File, 0o644);
    let ino2 = table.get_or_insert("README.md", InodeKind::File, 0o644);
    assert_eq!(ino1, ino2);
}

#[test]
fn different_paths_get_different_inodes() {
    let mut table = InodeTable::new();

    let a = table.get_or_insert("a.txt", InodeKind::File, 0o644);
    let b = table.get_or_insert("b.txt", InodeKind::File, 0o644);
    assert_ne!(a, b);
}

#[test]
fn forget_removes_inode_when_refcount_zero() {
    let mut table = InodeTable::new();

    let ino = table.get_or_insert("temp.txt", InodeKind::File, 0o644);
    assert!(table.get(ino).is_some());

    table.forget(ino, 1); // refcount was 1, now 0
    assert!(table.get(ino).is_none());
}

#[test]
fn forget_does_not_remove_with_remaining_refs() {
    let mut table = InodeTable::new();

    let ino = table.get_or_insert("multi.txt", InodeKind::File, 0o644);
    // Second lookup bumps refcount to 2
    table.get_or_insert("multi.txt", InodeKind::File, 0o644);

    table.forget(ino, 1); // refcount 2 → 1
    assert!(table.get(ino).is_some()); // still alive

    table.forget(ino, 1); // refcount 1 → 0
    assert!(table.get(ino).is_none()); // now gone
}

#[test]
fn root_inode_cannot_be_forgotten() {
    let mut table = InodeTable::new();

    table.forget(1, u64::MAX);
    assert!(table.get(1).is_some()); // root is immortal
}

#[test]
fn lookup_by_path() {
    let mut table = InodeTable::new();

    assert_eq!(table.get_by_path("."), Some(1)); // root
    assert_eq!(table.get_by_path("nope"), None);

    let ino = table.get_or_insert("found.txt", InodeKind::File, 0o644);
    assert_eq!(table.get_by_path("found.txt"), Some(ino));
}
