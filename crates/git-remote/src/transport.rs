//! Push/fetch implementation using git pack files + opendal.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use opendal::Operator;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::refs;

/// A remote backed by opendal (S3, FS, etc).
pub struct Remote {
    op: Operator,
    /// Local GIT_DIR (resolved from env or cwd).
    git_dir: PathBuf,
}

impl Remote {
    /// Parse an afs:// URL and build the remote.
    ///
    /// URL formats:
    ///   afs://<config-path>/<prefix>   — config file at <config-path>
    ///   afs://<prefix>                 — uses AFS_BACKEND_CONFIG env var
    pub async fn from_url(url: &str) -> Result<Self> {
        let path = url
            .strip_prefix("afs://")
            .unwrap_or(url);

        let backend_config = if let Ok(config_path) = std::env::var("AFS_BACKEND_CONFIG") {
            let content = std::fs::read_to_string(&config_path)
                .with_context(|| format!("read backend config: {}", config_path))?;
            let mut config: afs_store::backend::BackendConfig = toml::from_str(&content)?;
            // Use URL path as prefix
            if !path.is_empty() {
                config = with_prefix(config, path);
            }
            config
        } else {
            // Try to parse as "<config-path>/<prefix>"
            // Find the .toml file in the path
            let mut config_file = None;
            let mut prefix = path;
            for (i, _) in path.match_indices('/') {
                let candidate = &path[..i];
                if candidate.ends_with(".toml") && std::path::Path::new(candidate).exists() {
                    config_file = Some(candidate.to_string());
                    prefix = &path[i + 1..];
                    break;
                }
            }
            // Also check the whole path
            if config_file.is_none() && path.ends_with(".toml") && std::path::Path::new(path).exists() {
                config_file = Some(path.to_string());
                prefix = "";
            }

            match config_file {
                Some(cf) => {
                    let content = std::fs::read_to_string(&cf)?;
                    let mut config: afs_store::backend::BackendConfig = toml::from_str(&content)?;
                    if !prefix.is_empty() {
                        config = with_prefix(config, prefix);
                    }
                    config
                }
                None => {
                    // Default: FS backend with path as root
                    afs_store::backend::BackendConfig::Fs {
                        root: if path.is_empty() {
                            "/tmp/afs-remote".to_string()
                        } else {
                            path.to_string()
                        },
                    }
                }
            }
        };

        let op = afs_store::backend::create_operator(&backend_config)?;
        let git_dir = resolve_git_dir()?;

        Ok(Self { op, git_dir })
    }

    /// List refs on the remote.
    pub async fn list_refs(&self) -> Result<Vec<(String, String)>> {
        let refs = refs::read_refs(&self.op).await?;
        let mut result: Vec<(String, String)> = refs.into_iter().collect();

        // Also report HEAD if we have a main/master ref
        if let Some(main) = result.iter().find(|(r, _)| r == "refs/heads/main" || r == "refs/heads/master") {
            let head_target = main.0.clone();
            result.push(("@".to_string() + " " + &head_target + " HEAD", main.1.clone()));
        }

        Ok(result)
    }

    /// Push a refspec: "+<src>:<dst>" or "<src>:<dst>".
    pub async fn push(&self, spec: &str) -> Result<()> {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        let (src, dst) = spec
            .split_once(':')
            .context("invalid push spec, expected src:dst")?;

        // Handle delete
        if src.is_empty() {
            info!(%dst, "deleting remote ref");
            let mut remote_refs = refs::read_refs(&self.op).await?;
            remote_refs.remove(dst);
            refs::write_refs(&self.op, &remote_refs).await?;
            return Ok(());
        }

        // Resolve local ref to OID
        let local_oid = resolve_ref(&self.git_dir, src)?;
        info!(%src, %dst, %local_oid, "pushing");

        // Generate a pack containing all objects needed
        let pack_data = generate_pack(&self.git_dir, &local_oid, &self.op).await?;

        if !pack_data.is_empty() {
            let hash = sha256_hex(&pack_data);
            let key = format!("git/pack-{}.pack", hash);
            info!(key = %key, size = pack_data.len(), "uploading pack");
            self.op
                .write(&key, pack_data)
                .await
                .context("upload git pack")?;
        }

        // Update remote ref
        let mut remote_refs = refs::read_refs(&self.op).await?;
        remote_refs.insert(dst.to_string(), local_oid);
        refs::write_refs(&self.op, &remote_refs).await?;

        Ok(())
    }

    /// Fetch objects for a given oid + refname.
    pub async fn fetch(&self, spec: &str) -> Result<()> {
        let parts: Vec<&str> = spec.splitn(2, ' ').collect();
        if parts.len() < 2 {
            anyhow::bail!("invalid fetch spec: {}", spec);
        }
        let oid = parts[0];
        let refname = parts[1];
        info!(%oid, %refname, "fetching");

        // Track which packs we already fetched (marker dir inside GIT_DIR)
        let fetched_dir = self.git_dir.join("afs-fetched-packs");
        std::fs::create_dir_all(&fetched_dir).ok();

        let entries = self.op.list("git/").await.unwrap_or_default();
        for entry in entries {
            let key = entry.name();
            if !key.ends_with(".pack") {
                continue;
            }

            // Skip if already fetched
            let marker = fetched_dir.join(key);
            if marker.exists() {
                debug!(%key, "already fetched, skipping");
                continue;
            }

            let full_key = format!("git/{}", key);
            debug!(%full_key, "downloading pack");
            let pack_data = self.op.read(&full_key).await?.to_vec();

            // Feed pack to git index-pack
            let tmp = std::env::temp_dir().join(format!("afs-fetch-{}", key));
            std::fs::write(&tmp, &pack_data)?;

            let status = Command::new("git")
                .arg("--git-dir")
                .arg(&self.git_dir)
                .args(["index-pack", "--stdin"])
                .stdin(std::fs::File::open(&tmp)?)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .status()
                .context("git index-pack")?;

            let _ = std::fs::remove_file(&tmp);

            if !status.success() {
                anyhow::bail!("git index-pack failed for {}", key);
            }

            // Mark as fetched
            std::fs::write(&marker, "").ok();
        }

        Ok(())
    }
}

/// Resolve GIT_DIR from environment or by running git rev-parse.
fn resolve_git_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("GIT_DIR") {
        return Ok(PathBuf::from(d));
    }

    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .context("git rev-parse --git-dir")?;

    if !output.status.success() {
        anyhow::bail!("not in a git repository");
    }

    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(dir))
}

/// Resolve a ref (branch name or full refname) to an OID.
fn resolve_ref(git_dir: &PathBuf, refspec: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", refspec])
        .output()
        .context("git rev-parse")?;

    if !output.status.success() {
        anyhow::bail!("cannot resolve ref: {}", refspec);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Generate a git pack file for objects the remote doesn't have.
async fn generate_pack(
    git_dir: &PathBuf,
    oid: &str,
    op: &Operator,
) -> Result<Vec<u8>> {
    // Figure out what the remote already has
    let remote_refs = refs::read_refs(op).await?;
    let have_oids: Vec<String> = remote_refs.values().cloned().collect();

    // Use git rev-list to find objects to send
    let mut rev_list_args = vec!["--objects".to_string(), oid.to_string()];
    for have in &have_oids {
        rev_list_args.push(format!("^{}", have));
    }

    let rev_list = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("rev-list")
        .args(&rev_list_args)
        .output()
        .context("git rev-list")?;

    if !rev_list.status.success() || rev_list.stdout.is_empty() {
        debug!("no new objects to pack");
        return Ok(Vec::new());
    }

    // Pipe object list into git pack-objects
    let tmp_dir = std::env::temp_dir();
    let pack_base = tmp_dir.join("afs-push-pack");

    let mut pack_objects = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["pack-objects", "--stdout"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("git pack-objects")?;

    // Write object IDs (just the OIDs, one per line)
    {
        use std::io::Write;
        let stdin = pack_objects.stdin.as_mut().unwrap();
        for line in String::from_utf8_lossy(&rev_list.stdout).lines() {
            // rev-list --objects outputs "oid path" or just "oid"
            let obj_oid = line.split_whitespace().next().unwrap_or("");
            if !obj_oid.is_empty() {
                writeln!(stdin, "{}", obj_oid)?;
            }
        }
    }

    let output = pack_objects.wait_with_output().context("pack-objects")?;
    if !output.status.success() {
        anyhow::bail!("git pack-objects failed");
    }

    let pack_data = output.stdout;
    let object_count = rev_list.stdout.iter().filter(|&&b| b == b'\n').count();
    info!(objects = object_count, pack_size = pack_data.len(), "packed objects");

    let _ = std::fs::remove_file(pack_base.with_extension("pack"));
    let _ = std::fs::remove_file(pack_base.with_extension("idx"));

    Ok(pack_data)
}

/// Add a prefix to a backend config.
fn with_prefix(config: afs_store::backend::BackendConfig, prefix: &str) -> afs_store::backend::BackendConfig {
    match config {
        afs_store::backend::BackendConfig::S3 {
            bucket, region, endpoint, access_key_id, secret_access_key, prefix: existing,
        } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::S3 {
                bucket, region, endpoint, access_key_id, secret_access_key, prefix: new_prefix,
            }
        }
        afs_store::backend::BackendConfig::Gcs { bucket, credential, prefix: existing } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::Gcs { bucket, credential, prefix: new_prefix }
        }
        afs_store::backend::BackendConfig::AzBlob { container, account_name, account_key, prefix: existing } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::AzBlob { container, account_name, account_key, prefix: new_prefix }
        }
        afs_store::backend::BackendConfig::Fs { root } => {
            afs_store::backend::BackendConfig::Fs {
                root: format!("{}/{}", root.trim_end_matches('/'), prefix),
            }
        }
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}
