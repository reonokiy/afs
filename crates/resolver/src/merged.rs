use std::sync::atomic::{AtomicI64, Ordering};

use afs_db::{BaseNode, NodeKind, OverlayKind};
use anyhow::Result;
use sqlx::SqlitePool;

use crate::overlay::OverlayManager;
use crate::snapshot;

/// A resolved node — either from the base snapshot or overlay.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub path: String,
    pub kind: NodeKind,
    pub oid: Option<String>,
    pub mode: i64,
    pub size: Option<i64>,
    pub from_overlay: bool,
    /// For overlay files, the backing file path in upper/.
    pub backing_path: Option<String>,
}

impl ResolvedNode {
    pub fn from_base(node: BaseNode) -> Self {
        Self {
            path: node.path,
            kind: node.kind,
            oid: node.oid,
            mode: node.mode,
            size: node.size,
            from_overlay: false,
            backing_path: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == NodeKind::Symlink
    }

    /// Filename component of the path.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The Resolver merges snapshot (base_nodes) with overlay.
pub struct Resolver {
    pool: SqlitePool,
    generation: AtomicI64,
    overlay: Option<OverlayManager>,
}

impl Resolver {
    pub fn new(pool: SqlitePool, generation: i64) -> Self {
        Self {
            pool,
            generation: AtomicI64::new(generation),
            overlay: None,
        }
    }

    /// Set the overlay manager (called after overlay is initialized).
    pub fn set_overlay(&mut self, overlay: OverlayManager) {
        self.overlay = Some(overlay);
    }

    pub fn overlay(&self) -> Option<&OverlayManager> {
        self.overlay.as_ref()
    }

    pub fn generation(&self) -> i64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn set_generation(&self, generation: i64) {
        self.generation.store(generation, Ordering::Release);
    }

    /// Resolve a single path to a node.
    pub async fn resolve(&self, path: &str) -> Result<Option<ResolvedNode>> {
        let generation = self.generation();

        // Check overlay first
        if let Some(ref overlay) = self.overlay {
            if let Some(ovl) = overlay.get(path).await? {
                if ovl.is_deleted() {
                    return Ok(None); // Whiteout — file is deleted
                }

                let kind = match ovl.kind {
                    OverlayKind::Mkdir => NodeKind::Dir,
                    _ => NodeKind::Blob,
                };

                return Ok(Some(ResolvedNode {
                    path: ovl.path,
                    kind,
                    oid: ovl.source_oid,
                    mode: ovl.mode,
                    size: Some(ovl.size),
                    from_overlay: true,
                    backing_path: ovl.backing,
                }));
            }
        }

        // Fall back to base snapshot
        let base = snapshot::get_node(&self.pool, generation, path).await?;
        Ok(base.map(ResolvedNode::from_base))
    }

    /// List children of a directory path, merging snapshot + overlay.
    pub async fn list_dir(&self, parent: &str) -> Result<Vec<ResolvedNode>> {
        let generation = self.generation();

        // Get base children
        let base_children = snapshot::list_children(&self.pool, generation, parent).await?;
        let mut result: Vec<ResolvedNode> =
            base_children.into_iter().map(ResolvedNode::from_base).collect();

        // Merge overlay entries
        if let Some(ref overlay) = self.overlay {
            let overlay_entries = overlay.list_by_prefix(parent).await?;

            for ovl in overlay_entries {
                // Filter to direct children only
                let ovl_parent = afs_db::parent_dir(&ovl.path);
                if ovl_parent != parent {
                    continue;
                }

                if ovl.is_deleted() {
                    // Remove the base entry if it exists (whiteout)
                    result.retain(|r| r.path != ovl.path);
                    continue;
                }

                // Check if this overlays an existing base entry
                if let Some(existing) = result.iter_mut().find(|r| r.path == ovl.path) {
                    existing.from_overlay = true;
                    existing.size = Some(ovl.size);
                    existing.backing_path = ovl.backing;
                    if ovl.kind == OverlayKind::Mkdir {
                        existing.kind = NodeKind::Dir;
                    }
                } else {
                    // New entry from overlay
                    let kind = match ovl.kind {
                        OverlayKind::Mkdir => NodeKind::Dir,
                        _ => NodeKind::Blob,
                    };
                    result.push(ResolvedNode {
                        path: ovl.path,
                        kind,
                        oid: ovl.source_oid,
                        mode: ovl.mode,
                        size: Some(ovl.size),
                        from_overlay: true,
                        backing_path: ovl.backing,
                    });
                }
            }
        }

        Ok(result)
    }
}
