use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tracing::{debug, info};

/// Watches gitdir for HEAD/ref changes by polling file mtimes.
pub struct Watcher {
    head_path: PathBuf,
    ref_path: Option<PathBuf>,
    last_head_mtime: Option<SystemTime>,
    last_ref_mtime: Option<SystemTime>,
    poll_interval: Duration,
}

impl Watcher {
    pub fn new(gitdir: &Path) -> Self {
        Self {
            head_path: gitdir.join("HEAD"),
            ref_path: None,
            last_head_mtime: None,
            last_ref_mtime: None,
            poll_interval: Duration::from_millis(500),
        }
    }

    /// Check for changes. Returns true if HEAD or the tracked ref changed.
    pub fn poll(&mut self) -> Result<bool> {
        let mut changed = false;

        // Check HEAD file mtime
        if let Ok(meta) = std::fs::metadata(&self.head_path) {
            let mtime = meta.modified().ok();
            if self.last_head_mtime.is_some() && mtime != self.last_head_mtime {
                debug!("HEAD file changed");
                changed = true;
                // Re-read HEAD to find the ref it points to
                self.update_ref_path()?;
            }
            self.last_head_mtime = mtime;
        }

        // Check tracked ref file mtime
        if let Some(ref ref_path) = self.ref_path {
            if let Ok(meta) = std::fs::metadata(ref_path) {
                let mtime = meta.modified().ok();
                if self.last_ref_mtime.is_some() && mtime != self.last_ref_mtime {
                    debug!(?ref_path, "ref file changed");
                    changed = true;
                }
                self.last_ref_mtime = mtime;
            }
        }

        Ok(changed)
    }

    /// Initialize the watcher: read HEAD and prime the ref path.
    pub fn init(&mut self) -> Result<()> {
        self.last_head_mtime = std::fs::metadata(&self.head_path)
            .ok()
            .and_then(|m| m.modified().ok());
        self.update_ref_path()?;
        if let Some(ref ref_path) = self.ref_path {
            self.last_ref_mtime = std::fs::metadata(ref_path)
                .ok()
                .and_then(|m| m.modified().ok());
        }
        info!(?self.ref_path, "watcher initialized");
        Ok(())
    }

    fn update_ref_path(&mut self) -> Result<()> {
        let content = std::fs::read_to_string(&self.head_path)?;
        let content = content.trim();
        if let Some(refname) = content.strip_prefix("ref: ") {
            let ref_path = self.head_path.parent().unwrap().join(refname);
            self.ref_path = Some(ref_path);
        }
        Ok(())
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}
