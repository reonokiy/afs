//! git-lfs-afs-transfer: Custom LFS transfer adapter for afs.
//!
//! Implements the git-lfs custom transfer agent protocol:
//! https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md
//!
//! Reads/writes LFS objects directly from/to S3 via opendal,
//! using the same `lfs/{oid[0:2]}/{oid}` layout as afs-store.
//!
//! Configuration (in .lfsconfig or git config):
//!   [lfs]
//!     standalonetransferagent = afs
//!   [lfs "customtransfer.afs"]
//!     path = git-lfs-afs-transfer
//!     concurrent = true
//!     args = --backend-config /path/to/backend.toml
//!
//! Or set AFS_BACKEND_CONFIG environment variable.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use opendal::Operator;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(io::stderr)
        .init();

    let op = create_operator()?;
    let rt = tokio::runtime::Runtime::new()?;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        debug!(msg = %line, "received");

        let msg: Event = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %e, "failed to parse message, ignoring");
                continue;
            }
        };

        match msg.event.as_str() {
            "init" => {
                // Respond with empty JSON object to confirm init
                writeln!(out, "{{}}")?;
                out.flush()?;
            }
            "download" => {
                let oid = msg.oid.as_deref().unwrap_or("");
                let result = rt.block_on(download(&op, oid));
                match result {
                    Ok(path) => {
                        // Report progress
                        send(
                            &mut out,
                            &Response::complete(oid, Some(&path), None),
                        )?;
                    }
                    Err(e) => {
                        send(
                            &mut out,
                            &Response::error(oid, &format!("{:#}", e)),
                        )?;
                    }
                }
            }
            "upload" => {
                let oid = msg.oid.as_deref().unwrap_or("");
                let path = msg.path.as_deref().unwrap_or("");
                let size = msg.size.unwrap_or(0);
                let result = rt.block_on(upload(&op, oid, path, size));
                match result {
                    Ok(()) => {
                        send(&mut out, &Response::complete(oid, None, None))?;
                    }
                    Err(e) => {
                        send(
                            &mut out,
                            &Response::error(oid, &format!("{:#}", e)),
                        )?;
                    }
                }
            }
            "terminate" => break,
            other => {
                debug!(event = %other, "unknown event, ignoring");
            }
        }
    }

    Ok(())
}

// ── LFS transfer protocol types ───────────────────────────────

#[derive(Deserialize)]
struct Event {
    event: String,
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Serialize)]
struct Response {
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
}

#[derive(Serialize)]
struct ErrorInfo {
    code: i32,
    message: String,
}

impl Response {
    fn complete(oid: &str, path: Option<&str>, _error: Option<&str>) -> Self {
        Self {
            event: "complete".to_string(),
            oid: Some(oid.to_string()),
            path: path.map(|p| p.to_string()),
            error: None,
        }
    }

    fn error(oid: &str, message: &str) -> Self {
        Self {
            event: "complete".to_string(),
            oid: Some(oid.to_string()),
            path: None,
            error: Some(ErrorInfo {
                code: 2,
                message: message.to_string(),
            }),
        }
    }
}

fn send(out: &mut impl Write, resp: &Response) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    writeln!(out, "{}", json)?;
    out.flush()?;
    Ok(())
}

// ── S3 operations ──────────────────────────────────────────────

fn lfs_key(oid: &str) -> String {
    format!("lfs/{}/{}", &oid[..2], oid)
}

async fn download(op: &Operator, oid: &str) -> Result<String> {
    let key = lfs_key(oid);
    info!(%oid, %key, "downloading LFS object");

    let data = op.read(&key).await.context("read LFS object from S3")?;

    // Write to a temp file that git-lfs will pick up
    let tmp_dir = std::env::temp_dir().join("afs-lfs");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(oid);
    std::fs::write(&tmp_path, data.to_vec())?;

    Ok(tmp_path.to_str().unwrap().to_string())
}

async fn upload(op: &Operator, oid: &str, local_path: &str, size: u64) -> Result<()> {
    let key = lfs_key(oid);
    info!(%oid, %key, size, "uploading LFS object");

    let data = std::fs::read(local_path).context("read local LFS file")?;
    op.write(&key, data).await.context("write LFS object to S3")?;

    Ok(())
}

// ── Config ─────────────────────────────────────────────────────

fn create_operator() -> Result<Operator> {
    // Check --backend-config arg or AFS_BACKEND_CONFIG env
    let config_path = std::env::args()
        .skip_while(|a| a != "--backend-config")
        .nth(1)
        .or_else(|| std::env::var("AFS_BACKEND_CONFIG").ok());

    let backend_config = match config_path {
        Some(path) => {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("read backend config: {}", path))?;
            toml::from_str::<afs_store::backend::BackendConfig>(&content)?
        }
        None => {
            anyhow::bail!(
                "no backend config: set AFS_BACKEND_CONFIG env var or pass --backend-config"
            );
        }
    };

    afs_store::backend::create_operator(&backend_config)
}
