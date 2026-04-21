use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use afs_db::{BaseNode, NodeKind, OverlayKind, OverlayNode};
use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::info;

/// Manages the overlay layer: SQLite metadata + upper/ directory for file content.
pub struct OverlayManager {
    pool: SqlitePool,
    upper_dir: PathBuf,
}

impl OverlayManager {
    pub fn new(pool: SqlitePool, upper_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&upper_dir)?;
        Ok(Self { pool, upper_dir })
    }

    /// Get an overlay entry by path.
    pub async fn get(&self, path: &str) -> Result<Option<OverlayNode>> {
        Ok(afs_db::nodes::get_overlay_node(&self.pool, path).await?)
    }

    /// Ensure copy-on-write: promote a base file to the overlay.
    /// If already in overlay, return existing entry.
    /// `base_data` should be the hydrated blob content.
    pub async fn ensure_copy_on_write(
        &self,
        path: &str,
        base: &BaseNode,
        base_data: &[u8],
    ) -> Result<OverlayNode> {
        // Check if already in overlay
        if let Some(existing) = self.get(path).await?
            && !existing.is_deleted()
        {
            return Ok(existing);
        }

        let backing = self.backing_path(path);
        if let Some(parent) = backing.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write the base content to upper/
        std::fs::write(&backing, base_data)?;

        let now = now_ns();
        let node = OverlayNode {
            path: path.to_string(),
            kind: OverlayKind::Modify,
            backing: Some(backing.to_string_lossy().to_string()),
            mode: base.mode,
            size: base_data.len() as i64,
            mtime_ns: now,
            source_oid: base.oid.clone(),
        };

        afs_db::nodes::upsert_overlay_node(&self.pool, &node).await?;
        Ok(node)
    }

    /// Create a new file in the overlay.
    pub async fn create_file(&self, path: &str, mode: i64) -> Result<OverlayNode> {
        let backing = self.backing_path(path);
        if let Some(parent) = backing.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&backing, b"")?;

        let now = now_ns();
        let node = OverlayNode {
            path: path.to_string(),
            kind: OverlayKind::Create,
            backing: Some(backing.to_string_lossy().to_string()),
            mode,
            size: 0,
            mtime_ns: now,
            source_oid: None,
        };

        afs_db::nodes::upsert_overlay_node(&self.pool, &node).await?;
        Ok(node)
    }

    /// Write data to an overlay file at the given offset.
    pub async fn write_file(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        let node = self
            .get(path)
            .await?
            .context("overlay entry not found for write")?;

        if node.is_deleted() {
            anyhow::bail!("cannot write to deleted overlay entry");
        }

        let backing = node.backing.as_ref().context("no backing path")?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(backing)?;

        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(offset))?;
        let n = file.write(data)?;

        let size = std::fs::metadata(backing)?.len() as i64;
        let now = now_ns();

        let updated = OverlayNode {
            size,
            mtime_ns: now,
            ..node
        };
        afs_db::nodes::upsert_overlay_node(&self.pool, &updated).await?;

        Ok(n)
    }

    /// Mark a file as deleted (whiteout).
    pub async fn remove(&self, path: &str) -> Result<()> {
        // Remove backing file if exists
        if let Some(existing) = self.get(path).await?
            && let Some(ref backing) = existing.backing
        {
            let _ = std::fs::remove_file(backing);
        }

        let node = OverlayNode {
            path: path.to_string(),
            kind: OverlayKind::Delete,
            backing: None,
            mode: 0,
            size: 0,
            mtime_ns: now_ns(),
            source_oid: None,
        };
        afs_db::nodes::upsert_overlay_node(&self.pool, &node).await?;
        Ok(())
    }

    /// Create a directory in the overlay.
    pub async fn mkdir(&self, path: &str, mode: i64) -> Result<()> {
        let backing = self.backing_path(path);
        std::fs::create_dir_all(&backing)?;

        let node = OverlayNode {
            path: path.to_string(),
            kind: OverlayKind::Mkdir,
            backing: Some(backing.to_string_lossy().to_string()),
            mode,
            size: 0,
            mtime_ns: now_ns(),
            source_oid: None,
        };
        afs_db::nodes::upsert_overlay_node(&self.pool, &node).await?;
        Ok(())
    }

    /// Rename a file/directory in the overlay.
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old_node = self
            .get(old_path)
            .await?
            .context("rename: source not in overlay")?;

        if old_node.is_deleted() {
            anyhow::bail!("cannot rename deleted entry");
        }

        let new_backing = self.backing_path(new_path);
        if let Some(parent) = new_backing.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Move backing file
        if let Some(ref old_backing) = old_node.backing {
            std::fs::rename(old_backing, &new_backing)?;
        }

        // Record new entry
        let new_node = OverlayNode {
            path: new_path.to_string(),
            kind: OverlayKind::Rename,
            backing: Some(new_backing.to_string_lossy().to_string()),
            mode: old_node.mode,
            size: old_node.size,
            mtime_ns: now_ns(),
            source_oid: old_node.source_oid,
        };
        afs_db::nodes::upsert_overlay_node(&self.pool, &new_node).await?;

        // Record deletion of old path
        self.remove(old_path).await?;
        Ok(())
    }

    /// List overlay entries under a prefix (direct children).
    pub async fn list_by_prefix(&self, prefix: &str) -> Result<Vec<OverlayNode>> {
        Ok(afs_db::nodes::list_overlay_by_prefix(&self.pool, prefix).await?)
    }

    /// Reconcile overlay with a new base snapshot.
    pub async fn reconcile<F>(&self, base_lookup: F) -> Result<usize>
    where
        F: Fn(&str) -> Option<BaseNode>,
    {
        let all_entries = afs_db::nodes::list_overlay_by_prefix(&self.pool, ".").await?;
        let mut removed = 0;

        for entry in &all_entries {
            let base = base_lookup(&entry.path);
            let should_remove = match entry.kind {
                OverlayKind::Delete => base.is_none(),
                OverlayKind::Create => base.is_some(),
                OverlayKind::Mkdir => base.as_ref().is_some_and(|b| b.kind == NodeKind::Dir),
                OverlayKind::Modify | OverlayKind::Rename => {
                    match base {
                        None => true,
                        Some(ref b) => {
                            // Keep if base OID matches source_oid
                            entry.source_oid.as_deref() != b.oid.as_deref()
                        }
                    }
                }
            };

            if should_remove {
                if let Some(ref backing) = entry.backing {
                    let _ = std::fs::remove_file(backing);
                }
                afs_db::nodes::delete_overlay_node(&self.pool, &entry.path).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!(removed, "overlay reconciled");
        }
        Ok(removed)
    }

    /// Read the content of an overlay file.
    pub fn read_file(&self, backing_path: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(backing_path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn backing_path(&self, path: &str) -> PathBuf {
        self.upper_dir.join(path)
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}
