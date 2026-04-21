mod config;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "afs", about = "Git filesystem with S3-backed blob storage")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the FUSE daemon (run first, stays in foreground)
    Daemon {
        /// Number of hydration workers
        #[arg(long, default_value = "4")]
        workers: usize,
    },
    /// Clone a repo (blobless) and register it — daemon auto-mounts it
    Clone {
        /// Git remote URL
        remote: String,
        /// Mount point path
        mount_path: PathBuf,
        /// Branch to track
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Show repo status
    Status {
        /// Repo name (derived from mount path)
        name: Option<String>,
    },
    /// Push local blob changes to S3
    Push {
        /// Repo name
        name: String,
        /// Storage backend config file (TOML)
        #[arg(long)]
        backend_config: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = config::Config::load().unwrap_or_else(|_| config::Config {
        data_root: std::env::var("AFS_DATA_ROOT")
            .map(PathBuf::from)
            .ok()
            .or_else(|| dirs::data_dir().map(|d| d.join("afs")))
            .unwrap_or_else(|| PathBuf::from("/var/lib/afs")),
        pack: Default::default(),
        cache: Default::default(),
    });

    match cli.command {
        Command::Daemon { workers } => cmd_daemon(&config, workers),
        Command::Clone {
            remote,
            mount_path,
            branch,
        } => cmd_clone(&config, &remote, &mount_path, &branch),
        Command::Status { name } => cmd_status(&config, name.as_deref()),
        Command::Push {
            name,
            backend_config,
        } => cmd_push(&config, &name, backend_config.as_deref()),
    }
}

// ── clone: register a repo for the daemon to mount ──────────────

fn cmd_clone(
    config: &config::Config,
    remote: &str,
    mount_path: &std::path::Path,
    branch: &str,
) -> Result<()> {
    let mount_path = std::fs::canonicalize(mount_path.parent().context("mount path has no parent")?)
        .unwrap_or_else(|_| mount_path.parent().unwrap().to_path_buf())
        .join(mount_path.file_name().context("mount path has no filename")?);

    let name = mount_path
        .file_name()
        .context("mount path has no filename")?
        .to_str()
        .context("non-utf8 mount path")?;

    let repo_dir = config.data_root.join("repos").join(name);
    let gitdir = repo_dir.join("gitdir");

    info!(%remote, %branch, ?mount_path, "cloning repo");

    afs_indexer::blobless_clone(remote, branch, &gitdir)?;

    // Build initial tree index
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let db_path = repo_dir.join("snapshot.db");
        let pool = afs_db::schema::open_db(db_path.to_str().unwrap()).await?;
        let nodes = afs_indexer::build_tree_index(&gitdir, 1)?;
        afs_db::nodes::publish_generation(&pool, 1, &nodes).await?;
        info!(count = nodes.len(), "tree indexed");
        Ok::<_, anyhow::Error>(())
    })?;

    // Save repo config — daemon watches for these and auto-mounts
    let mount_path_str = mount_path.display().to_string();
    let repo_config = format!(
        "remote = {remote:?}\nbranch = {branch:?}\nmount_path = {mount_path_str:?}\n",
    );
    std::fs::create_dir_all(&repo_dir)?;
    std::fs::write(repo_dir.join("repo.toml"), repo_config)?;

    info!(%name, ?mount_path, "repo registered — daemon will auto-mount it");
    Ok(())
}

// ── daemon: long-running service, watches for repos and mounts them ──

fn cmd_daemon(config: &config::Config, _workers: usize) -> Result<()> {
    use notify::{EventKind, RecursiveMode, Watcher};
    use std::sync::mpsc;

    let repos_dir = config.data_root.join("repos");
    std::fs::create_dir_all(&repos_dir)?;

    let rt = tokio::runtime::Runtime::new()?;
    let mut mounted: HashSet<String> = HashSet::new();
    let mut mount_handles: Vec<fuser::BackgroundSession> = Vec::new();

    // Initial scan — mount any already-registered repos
    scan_and_mount(&repos_dir, &mut mounted, &mut mount_handles, &rt);

    // Set up filesystem watcher on repos dir
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Only care about new files/dirs being created (repo.toml appearing)
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    let _ = tx.send(());
                }
                _ => {}
            }
        }
    })?;

    watcher.watch(&repos_dir, RecursiveMode::Recursive)?;
    info!(?repos_dir, "daemon started, watching for new repos (inotify)");

    // Block on events
    loop {
        // Wait for a filesystem event (or check every 30s as fallback)
        let _ = rx.recv_timeout(std::time::Duration::from_secs(30));
        scan_and_mount(&repos_dir, &mut mounted, &mut mount_handles, &rt);
    }
}

fn scan_and_mount(
    repos_dir: &std::path::Path,
    mounted: &mut HashSet<String>,
    mount_handles: &mut Vec<fuser::BackgroundSession>,
    rt: &tokio::runtime::Runtime,
) {
    let entries = match std::fs::read_dir(repos_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let repo_dir = entry.path();
        if !repo_dir.is_dir() {
            continue;
        }

        let name = repo_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        if mounted.contains(&name) {
            continue;
        }

        let config_path = repo_dir.join("repo.toml");
        if !config_path.exists() {
            continue;
        }

        match mount_repo(&repo_dir, rt) {
            Ok(handle) => {
                mount_handles.push(handle);
                mounted.insert(name.clone());
                info!(%name, "repo mounted");
            }
            Err(e) => {
                warn!(%name, error = %e, "failed to mount repo");
            }
        }
    }
}

fn mount_repo(
    repo_dir: &std::path::Path,
    rt: &tokio::runtime::Runtime,
) -> Result<fuser::BackgroundSession> {
    let config_path = repo_dir.join("repo.toml");
    let repo_config: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path)?)?;

    let mount_path_str = repo_config
        .get("mount_path")
        .and_then(|v| v.as_str())
        .context("missing mount_path in repo config")?;
    let mount_path = PathBuf::from(mount_path_str);

    let name = repo_dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();

    let gitdir = repo_dir.join("gitdir");
    let db_path = repo_dir.join("snapshot.db");

    std::fs::create_dir_all(&mount_path)?;

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

    let fs = afs_fuse::AfsFilesystem::new(resolver, gitdir, rt.handle().clone());

    let mut fuse_config = fuser::Config::default();
    fuse_config.mount_options = vec![
        fuser::MountOption::FSName(format!("afs:{}", name)),
    ];

    info!(%name, ?mount_path, generation, "mounting FUSE");

    let session = fuser::spawn_mount2(fs, &mount_path, &fuse_config)?;
    Ok(session)
}

// ── status ──────────────────────────────────────────────────────

fn cmd_status(config: &config::Config, name: Option<&str>) -> Result<()> {
    let repos_dir = config.data_root.join("repos");
    if !repos_dir.exists() {
        println!("No repos registered.");
        return Ok(());
    }

    for entry in std::fs::read_dir(&repos_dir)? {
        let entry = entry?;
        let repo_dir = entry.path();
        if !repo_dir.is_dir() {
            continue;
        }
        let repo_name = repo_dir.file_name().unwrap().to_str().unwrap();
        if let Some(filter) = name
            && repo_name != filter
        {
            continue;
        }

        let config_path = repo_dir.join("repo.toml");
        if !config_path.exists() {
            continue;
        }

        let gitdir = repo_dir.join("gitdir");
        let (head_oid, head_ref) = afs_indexer::resolve_head(&gitdir)
            .unwrap_or_else(|_| ("unknown".into(), "unknown".into()));

        println!("{repo_name}  head={head_oid:.8}  ref={head_ref}");
    }

    Ok(())
}

// ── push ────────────────────────────────────────────────────────

fn cmd_push(
    config: &config::Config,
    name: &str,
    backend_config_path: Option<&std::path::Path>,
) -> Result<()> {
    let repo_dir = config.data_root.join("repos").join(name);
    if !repo_dir.exists() {
        anyhow::bail!("repo '{}' not found", name);
    }

    let gitdir = repo_dir.join("gitdir");
    let db_path = repo_dir.join("snapshot.db");

    let backend_config = match backend_config_path {
        Some(p) => {
            let content = std::fs::read_to_string(p)?;
            toml::from_str::<afs_store::backend::BackendConfig>(&content)?
        }
        None => afs_store::backend::BackendConfig::default(),
    };

    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        let pool = afs_db::schema::open_db(db_path.to_str().unwrap()).await?;

        let (head_oid, _head_ref) = afs_indexer::resolve_head(&gitdir)?;

        let last_synced = afs_db::packs::get_sync_state(&pool, "last_synced_oid").await?;

        if last_synced.as_deref() == Some(&head_oid) {
            info!("already synced to {}", &head_oid[..8]);
            return Ok(());
        }

        let diff_range = match &last_synced {
            Some(old) => format!("{}..{}", old, head_oid),
            None => head_oid.clone(),
        };

        info!(range = %diff_range, "finding new blobs to push");

        let output = std::process::Command::new("git")
            .env("GIT_DIR", &gitdir)
            .args([
                "diff-tree",
                "-r",
                "--diff-filter=AM",
                "--no-commit-id",
                &diff_range,
            ])
            .output()
            .context("git diff-tree")?;

        let mut new_oids: Vec<(String, String)> = Vec::new();

        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 5 {
                    let new_oid = parts[3].to_string();
                    let path = parts.last().unwrap().to_string();
                    if new_oid != "0000000000000000000000000000000000000000" {
                        new_oids.push((new_oid, path));
                    }
                }
            }
        } else if last_synced.is_none() {
            let output = std::process::Command::new("git")
                .env("GIT_DIR", &gitdir)
                .args(["ls-tree", "-r", "HEAD"])
                .output()
                .context("git ls-tree")?;

            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 && parts[1] == "blob" {
                    let oid = parts[2].to_string();
                    let path = parts[3..].join(" ");
                    new_oids.push((oid, path));
                }
            }
        }

        if new_oids.is_empty() {
            info!("no new blobs to push");
            afs_db::packs::set_sync_state(&pool, "last_synced_oid", &head_oid).await?;
            return Ok(());
        }

        info!(count = new_oids.len(), "pushing blobs to S3");

        let store = Arc::new(
            afs_store::BlobStore::new(&backend_config, &config.cache, pool.clone()).await?,
        );

        let repo = gix::open(&gitdir)?;
        let mut small_blobs: Vec<afs_store::pack::PackEntryData> = Vec::new();
        let mut large_blobs: Vec<(String, Vec<u8>)> = Vec::new();
        let mut pack_upload_handles: Vec<tokio::task::JoinHandle<Result<String>>> = Vec::new();

        for (oid, path) in &new_oids {
            let gix_oid = gix::ObjectId::from_hex(oid.as_bytes())?;
            let obj = match repo.find_object(gix_oid) {
                Ok(o) => o,
                Err(_) => {
                    warn!(%oid, %path, "blob not found in local git, skipping");
                    continue;
                }
            };
            let data = obj.data.to_vec();

            if data.len() < config.pack.pack_threshold {
                small_blobs.push(afs_store::pack::PackEntryData {
                    oid: oid.clone(),
                    data,
                });
            } else {
                large_blobs.push((oid.clone(), data));
            }

            // Optimization 2: when a pack batch is ready, spawn upload as background task
            if small_blobs.len() >= 1000 {
                let batch = std::mem::take(&mut small_blobs);
                let store_ref = store.clone();
                let count = batch.len();
                pack_upload_handles.push(tokio::spawn(async move {
                    let pack_id = store_ref.put_pack(&batch).await?;
                    info!(pack_id = %&pack_id[..8], entries = count, "uploaded pack");
                    Ok(pack_id)
                }));
            }
        }

        // Optimization 1: upload large blobs concurrently (limit to 8)
        let large_count = large_blobs.len() as u32;
        let large_results: Vec<Result<()>> = stream::iter(large_blobs)
            .map(|(oid, data)| {
                let store_ref = store.clone();
                async move {
                    store_ref.put_blob(&oid, &data).await?;
                    Ok(())
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;

        // Check for errors in large blob uploads
        for result in large_results {
            result?;
        }

        // Upload remaining small blobs as a final pack
        if !small_blobs.is_empty() {
            let batch = small_blobs;
            let store_ref = store.clone();
            let count = batch.len();
            pack_upload_handles.push(tokio::spawn(async move {
                let pack_id = store_ref.put_pack(&batch).await?;
                info!(pack_id = %&pack_id[..8], entries = count, "uploaded pack");
                Ok(pack_id)
            }));
        }

        // Await all background pack uploads
        for handle in pack_upload_handles {
            handle.await.context("pack upload task panicked")??;
        }

        afs_db::packs::set_sync_state(&pool, "last_synced_oid", &head_oid).await?;

        info!(
            large_blobs = large_count,
            total = new_oids.len(),
            "push complete"
        );

        Ok(())
    })
}
