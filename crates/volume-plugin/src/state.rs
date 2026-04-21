use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;

/// Per-volume runtime state.
pub struct VolumeState {
    pub name: String,
    pub remote: String,
    pub branch: String,
    pub mountpoint: PathBuf,
    pub repo_dir: PathBuf,
    /// Number of active container mounts.
    refcount: AtomicU32,
    /// FUSE background session — dropped to unmount.
    pub mount_session: std::sync::Mutex<Option<fuser::BackgroundSession>>,
}

impl VolumeState {
    pub fn new(
        name: String,
        remote: String,
        branch: String,
        mountpoint: PathBuf,
        repo_dir: PathBuf,
    ) -> Self {
        Self {
            name,
            remote,
            branch,
            mountpoint,
            repo_dir,
            refcount: AtomicU32::new(0),
            mount_session: std::sync::Mutex::new(None),
        }
    }

    pub fn increment(&self) -> u32 {
        self.refcount.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn decrement(&self) -> u32 {
        let prev = self.refcount.fetch_sub(1, Ordering::AcqRel);
        prev.saturating_sub(1)
    }

    pub fn refcount(&self) -> u32 {
        self.refcount.load(Ordering::Acquire)
    }

    pub fn is_mounted(&self) -> bool {
        self.mount_session.lock().unwrap().is_some()
    }
}

/// Shared plugin state across all handlers.
pub struct PluginState {
    pub volumes: DashMap<String, VolumeState>,
    pub data_root: PathBuf,
}

impl PluginState {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            volumes: DashMap::new(),
            data_root,
        }
    }

    pub fn repo_dir(&self, name: &str) -> PathBuf {
        self.data_root.join("repos").join(name)
    }

    pub fn mountpoint(&self, name: &str) -> PathBuf {
        self.data_root.join("mounts").join(name)
    }

    /// Scan existing repos on disk and restore volume state.
    pub fn restore_from_disk(&self) {
        let repos_dir = self.data_root.join("repos");
        let entries = match std::fs::read_dir(&repos_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let repo_dir = entry.path();
            if !repo_dir.is_dir() {
                continue;
            }

            let config_path = repo_dir.join("repo.toml");
            if !config_path.exists() {
                continue;
            }

            let name = repo_dir
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            let config: toml::Value = match std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
            {
                Some(c) => c,
                None => continue,
            };

            let remote = config
                .get("remote")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let branch = config
                .get("branch")
                .and_then(|v| v.as_str())
                .unwrap_or("main")
                .to_string();

            let mountpoint = self.mountpoint(&name);
            let state = VolumeState::new(
                name.clone(),
                remote,
                branch,
                mountpoint,
                repo_dir,
            );
            self.volumes.insert(name, state);
        }
    }

    /// Mount a volume's FUSE filesystem.
    pub fn mount_fuse(
        &self,
        name: &str,
        rt: &tokio::runtime::Handle,
    ) -> anyhow::Result<()> {
        let vol = self
            .volumes
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("volume '{}' not found", name))?;

        if vol.is_mounted() {
            return Ok(());
        }

        let repo_dir = &vol.repo_dir;
        let gitdir = repo_dir.join("gitdir");
        let db_path = repo_dir.join("snapshot.db");
        let mountpoint = &vol.mountpoint;

        std::fs::create_dir_all(mountpoint)?;

        let pool = rt.block_on(afs_db::schema::open_db(db_path.to_str().unwrap()))?;

        let generation = rt.block_on(async {
            let row = sqlx::query!("SELECT MAX(gen) as max_gen FROM base_nodes")
                .fetch_one(&pool)
                .await?;
            match row.max_gen {
                Some(g) => Ok::<_, anyhow::Error>(g),
                None => {
                    let nodes = afs_indexer::build_tree_index(&gitdir, 1)?;
                    afs_db::nodes::publish_generation(&pool, 1, &nodes).await?;
                    Ok(1)
                }
            }
        })?;

        let upper_dir = repo_dir.join("upper");
        let overlay = afs_resolver::OverlayManager::new(pool.clone(), upper_dir)?;
        let mut resolver = afs_resolver::Resolver::new(pool, generation);
        resolver.set_overlay(overlay);

        let fs = afs_fuse::AfsFilesystem::new(resolver, gitdir, rt.clone());

        let fuse_config = fuser::Config::default();
        let session = fuser::spawn_mount2(fs, mountpoint, &fuse_config)?;

        *vol.mount_session.lock().unwrap() = Some(session);

        Ok(())
    }

    /// Unmount a volume's FUSE filesystem.
    pub fn unmount_fuse(&self, name: &str) {
        if let Some(vol) = self.volumes.get(name) {
            // Drop the session to unmount
            *vol.mount_session.lock().unwrap() = None;
        }
    }
}
