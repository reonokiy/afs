mod handler;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::post;
use clap::Parser;
use tokio::net::UnixListener;
use tracing::info;

#[derive(Parser)]
#[command(name = "afs-volume-plugin", about = "Docker/Podman volume plugin for afs")]
struct Cli {
    /// Unix socket path for the plugin.
    #[arg(long, default_value = "/run/docker/plugins/afs.sock")]
    socket: PathBuf,

    /// Data root directory for volumes.
    #[arg(long, default_value = "/var/lib/afs")]
    data_root: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Ensure directories exist
    std::fs::create_dir_all(&cli.data_root).context("create data root")?;
    std::fs::create_dir_all(cli.data_root.join("repos")).context("create repos dir")?;
    std::fs::create_dir_all(cli.data_root.join("mounts")).context("create mounts dir")?;

    if let Some(parent) = cli.socket.parent() {
        std::fs::create_dir_all(parent).context("create socket parent dir")?;
    }

    // Remove stale socket file
    let _ = std::fs::remove_file(&cli.socket);

    // Initialize state and restore existing volumes
    let plugin_state = Arc::new(state::PluginState::new(cli.data_root));
    plugin_state.restore_from_disk();

    info!(
        socket = %cli.socket.display(),
        volumes = plugin_state.volumes.len(),
        "afs volume plugin starting"
    );

    let app = Router::new()
        .route("/Plugin.Activate", post(handler::activate))
        .route("/VolumeDriver.Create", post(handler::create))
        .route("/VolumeDriver.Remove", post(handler::remove))
        .route("/VolumeDriver.Mount", post(handler::mount))
        .route("/VolumeDriver.Unmount", post(handler::unmount))
        .route("/VolumeDriver.Path", post(handler::path))
        .route("/VolumeDriver.Get", post(handler::get))
        .route("/VolumeDriver.List", post(handler::list))
        .route("/VolumeDriver.Capabilities", post(handler::capabilities))
        .with_state(plugin_state);

    let listener = UnixListener::bind(&cli.socket).context("bind unix socket")?;

    // Make socket world-accessible so Docker/Podman can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli.socket, std::fs::Permissions::from_mode(0o660))?;
    }

    info!(socket = %cli.socket.display(), "listening");

    axum::serve(listener, app)
        .await
        .context("axum serve")?;

    Ok(())
}
