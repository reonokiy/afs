pub mod nodes;
pub mod packs;
pub mod schema;

use std::fmt;
use std::str::FromStr;

/// Node kind in base_nodes table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NodeKind {
    Dir = 0,
    Blob = 1,
    Lfs = 2,
    Symlink = 3,
}

impl TryFrom<i32> for NodeKind {
    type Error = ();

    fn try_from(v: i32) -> Result<Self, ()> {
        match v {
            0 => Ok(Self::Dir),
            1 => Ok(Self::Blob),
            2 => Ok(Self::Lfs),
            3 => Ok(Self::Symlink),
            _ => Err(()),
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dir => write!(f, "dir"),
            Self::Blob => write!(f, "blob"),
            Self::Lfs => write!(f, "lfs"),
            Self::Symlink => write!(f, "symlink"),
        }
    }
}

/// Overlay entry kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKind {
    Create,
    Modify,
    Delete,
    Rename,
    Mkdir,
}

impl OverlayKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
            Self::Rename => "rename",
            Self::Mkdir => "mkdir",
        }
    }
}

impl FromStr for OverlayKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "create" => Ok(Self::Create),
            "modify" => Ok(Self::Modify),
            "delete" => Ok(Self::Delete),
            "rename" => Ok(Self::Rename),
            "mkdir" => Ok(Self::Mkdir),
            _ => Err(()),
        }
    }
}

impl fmt::Display for OverlayKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row from the overlay_nodes table.
#[derive(Debug, Clone)]
pub struct OverlayNode {
    pub path: String,
    pub kind: OverlayKind,
    pub backing: Option<String>,
    pub mode: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub source_oid: Option<String>,
}

impl OverlayNode {
    pub fn is_deleted(&self) -> bool {
        self.kind == OverlayKind::Delete
    }
}

/// A row from the base_nodes table.
#[derive(Debug, Clone)]
pub struct BaseNode {
    pub generation: i64,
    pub path: String,
    pub parent: String,
    pub kind: NodeKind,
    pub oid: Option<String>,
    pub mode: i64,
    pub size: Option<i64>,
}

/// Normalize a path for use as DB key: strip leading `/`, clean `.` segments.
pub fn clean_path(path: &str) -> String {
    let path = path.trim_start_matches('/');
    if path.is_empty() || path == "." || path == "/" {
        return ".".to_string();
    }
    path.trim_end_matches('/').to_string()
}

/// Return the parent directory of a path, using "." for root.
pub fn parent_dir(path: &str) -> String {
    if path == "." || path.is_empty() {
        return String::new();
    }
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}
