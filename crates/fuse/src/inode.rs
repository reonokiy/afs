use std::collections::HashMap;

use fuser::INodeNo;

/// Metadata cached per inode.
#[derive(Debug, Clone)]
pub struct InodeRef {
    pub path: String,
    pub kind: InodeKind,
    pub mode: u32,
    pub refcount: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeKind {
    Dir,
    File,
    Symlink,
}

/// Bidirectional mapping between inode IDs and paths.
pub struct InodeTable {
    next_ino: u64,
    by_ino: HashMap<u64, InodeRef>,
    by_path: HashMap<String, u64>,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut table = Self {
            next_ino: INodeNo::ROOT.0 + 1,
            by_ino: HashMap::new(),
            by_path: HashMap::new(),
        };
        // Pin root inode
        table.by_ino.insert(
            INodeNo::ROOT.0,
            InodeRef {
                path: ".".to_string(),
                kind: InodeKind::Dir,
                mode: 0o755,
                refcount: u64::MAX, // never evicted
            },
        );
        table.by_path.insert(".".to_string(), INodeNo::ROOT.0);
        table
    }

    /// Get or allocate an inode for a path.
    pub fn get_or_insert(&mut self, path: &str, kind: InodeKind, mode: u32) -> u64 {
        if let Some(&ino) = self.by_path.get(path) {
            if let Some(r) = self.by_ino.get_mut(&ino) {
                r.refcount = r.refcount.saturating_add(1);
            }
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(
            ino,
            InodeRef {
                path: path.to_string(),
                kind,
                mode,
                refcount: 1,
            },
        );
        self.by_path.insert(path.to_string(), ino);
        ino
    }

    /// Look up an inode by ID.
    pub fn get(&self, ino: u64) -> Option<&InodeRef> {
        self.by_ino.get(&ino)
    }

    /// Look up an inode by path.
    pub fn get_by_path(&self, path: &str) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    /// Decrease refcount; remove if it hits zero.
    pub fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == INodeNo::ROOT.0 {
            return;
        }
        if let Some(r) = self.by_ino.get_mut(&ino) {
            r.refcount = r.refcount.saturating_sub(nlookup);
            if r.refcount == 0 {
                let path = r.path.clone();
                self.by_ino.remove(&ino);
                self.by_path.remove(&path);
            }
        }
    }
}
