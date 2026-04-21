use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::state::{PluginState, VolumeState};

// ── Request/Response types ─────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateRequest {
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "Opts", default)]
    pub opts: HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct MountRequest {
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "ID")]
    pub id: String,
}

#[derive(Deserialize)]
pub struct UnmountRequest {
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "ID")]
    pub id: String,
}

#[derive(Deserialize)]
pub struct NameRequest {
    #[serde(alias = "Name")]
    pub name: String,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "Err")]
    pub err: String,
}

#[derive(Serialize)]
pub struct MountResponse {
    #[serde(rename = "Mountpoint")]
    pub mountpoint: String,
    #[serde(rename = "Err")]
    pub err: String,
}

#[derive(Serialize)]
pub struct GetResponse {
    #[serde(rename = "Volume", skip_serializing_if = "Option::is_none")]
    pub volume: Option<VolumeInfo>,
    #[serde(rename = "Err")]
    pub err: String,
}

#[derive(Serialize)]
pub struct VolumeInfo {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Mountpoint")]
    pub mountpoint: String,
    #[serde(rename = "Status")]
    pub status: HashMap<String, String>,
}

#[derive(Serialize)]
pub struct ListResponse {
    #[serde(rename = "Volumes")]
    pub volumes: Vec<VolumeInfo>,
    #[serde(rename = "Err")]
    pub err: String,
}

#[derive(Serialize)]
pub struct CapabilitiesResponse {
    #[serde(rename = "Capabilities")]
    pub capabilities: CapabilitiesInfo,
}

#[derive(Serialize)]
pub struct CapabilitiesInfo {
    #[serde(rename = "Scope")]
    pub scope: String,
}

#[derive(Serialize)]
pub struct ActivateResponse {
    #[serde(rename = "Implements")]
    pub implements: Vec<String>,
}

// ── Handlers ───────────────────────────────────────────────────

pub async fn activate() -> Json<ActivateResponse> {
    Json(ActivateResponse {
        implements: vec!["VolumeDriver".to_string()],
    })
}

pub async fn capabilities() -> Json<CapabilitiesResponse> {
    Json(CapabilitiesResponse {
        capabilities: CapabilitiesInfo {
            scope: "local".to_string(),
        },
    })
}

pub async fn create(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<CreateRequest>,
) -> Json<ErrorResponse> {
    info!(name = %req.name, opts = ?req.opts, "VolumeDriver.Create");

    let remote = match req.opts.get("remote") {
        Some(r) => r.clone(),
        None => {
            return Json(ErrorResponse {
                err: "missing required option 'remote'".to_string(),
            });
        }
    };

    let branch = req.opts.get("branch").cloned().unwrap_or("main".to_string());

    // Check if volume already exists
    if state.volumes.contains_key(&req.name) {
        return Json(ErrorResponse { err: String::new() });
    }

    let repo_dir = state.repo_dir(&req.name);
    let gitdir = repo_dir.join("gitdir");

    // Blobless clone
    if let Err(e) = afs_indexer::blobless_clone(&remote, &branch, &gitdir) {
        warn!(name = %req.name, error = %e, "clone failed");
        return Json(ErrorResponse {
            err: format!("clone failed: {}", e),
        });
    }

    // Build initial tree index
    let db_path = repo_dir.join("snapshot.db");
    let index_result: anyhow::Result<()> = async {
        let pool = afs_db::schema::open_db(db_path.to_str().unwrap()).await?;
        let nodes = afs_indexer::build_tree_index(&gitdir, 1)?;
        afs_db::nodes::publish_generation(&pool, 1, &nodes).await?;
        info!(name = %req.name, count = nodes.len(), "tree indexed");
        Ok(())
    }
    .await;

    if let Err(e) = index_result {
        warn!(name = %req.name, error = %e, "index failed");
        return Json(ErrorResponse {
            err: format!("index failed: {}", e),
        });
    }

    // Save repo config
    let repo_config = format!(
        "remote = {:?}\nbranch = {:?}\n",
        remote, branch,
    );
    let _ = std::fs::create_dir_all(&repo_dir);
    if let Err(e) = std::fs::write(repo_dir.join("repo.toml"), repo_config) {
        return Json(ErrorResponse {
            err: format!("write config: {}", e),
        });
    }

    let mountpoint = state.mountpoint(&req.name);
    let vol = VolumeState::new(
        req.name.clone(),
        remote,
        branch,
        mountpoint,
        repo_dir,
    );
    state.volumes.insert(req.name, vol);

    Json(ErrorResponse { err: String::new() })
}

pub async fn remove(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<NameRequest>,
) -> Json<ErrorResponse> {
    info!(name = %req.name, "VolumeDriver.Remove");

    // Unmount if mounted
    state.unmount_fuse(&req.name);

    // Remove from state
    if let Some((_, vol)) = state.volumes.remove(&req.name) {
        // Cleanup on disk
        let _ = std::fs::remove_dir_all(&vol.repo_dir);
        let _ = std::fs::remove_dir_all(&vol.mountpoint);
    }

    Json(ErrorResponse { err: String::new() })
}

pub async fn mount(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<MountRequest>,
) -> Json<MountResponse> {
    info!(name = %req.name, id = %req.id, "VolumeDriver.Mount");

    let vol = match state.volumes.get(&req.name) {
        Some(v) => v,
        None => {
            return Json(MountResponse {
                mountpoint: String::new(),
                err: format!("volume '{}' not found", req.name),
            });
        }
    };

    let mountpoint = vol.mountpoint.display().to_string();
    vol.increment();

    // Mount FUSE if not already mounted
    if !vol.is_mounted() {
        drop(vol); // release DashMap ref before mount_fuse borrows state
        let rt = tokio::runtime::Handle::current();
        if let Err(e) = state.mount_fuse(&req.name, &rt) {
            warn!(name = %req.name, error = %e, "mount failed");
            return Json(MountResponse {
                mountpoint: String::new(),
                err: format!("mount failed: {}", e),
            });
        }
    }

    Json(MountResponse {
        mountpoint,
        err: String::new(),
    })
}

pub async fn unmount(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<UnmountRequest>,
) -> Json<ErrorResponse> {
    info!(name = %req.name, id = %req.id, "VolumeDriver.Unmount");

    if let Some(vol) = state.volumes.get(&req.name) {
        let remaining = vol.decrement();
        info!(name = %req.name, remaining, "unmount refcount");

        if remaining == 0 {
            drop(vol);
            state.unmount_fuse(&req.name);
        }
    }

    Json(ErrorResponse { err: String::new() })
}

pub async fn path(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<NameRequest>,
) -> Json<MountResponse> {
    let mountpoint = state
        .volumes
        .get(&req.name)
        .map(|v| v.mountpoint.display().to_string())
        .unwrap_or_default();

    Json(MountResponse {
        mountpoint,
        err: String::new(),
    })
}

pub async fn get(
    State(state): State<Arc<PluginState>>,
    Json(req): Json<NameRequest>,
) -> Json<GetResponse> {
    match state.volumes.get(&req.name) {
        Some(vol) => {
            let mut status = HashMap::new();
            status.insert("remote".to_string(), vol.remote.clone());
            status.insert("branch".to_string(), vol.branch.clone());
            status.insert("refcount".to_string(), vol.refcount().to_string());
            status.insert("mounted".to_string(), vol.is_mounted().to_string());

            Json(GetResponse {
                volume: Some(VolumeInfo {
                    name: vol.name.clone(),
                    mountpoint: vol.mountpoint.display().to_string(),
                    status,
                }),
                err: String::new(),
            })
        }
        None => Json(GetResponse {
            volume: None,
            err: format!("volume '{}' not found", req.name),
        }),
    }
}

pub async fn list(State(state): State<Arc<PluginState>>) -> Json<ListResponse> {
    let volumes = state
        .volumes
        .iter()
        .map(|entry| {
            let vol = entry.value();
            VolumeInfo {
                name: vol.name.clone(),
                mountpoint: vol.mountpoint.display().to_string(),
                status: HashMap::new(),
            }
        })
        .collect();

    Json(ListResponse {
        volumes,
        err: String::new(),
    })
}
