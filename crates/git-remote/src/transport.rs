//! Push/fetch implementation using git pack files + opendal.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use opendal::Operator;
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
    pub async fn from_url(url: &str) -> Result<Self> {
        let path = url.strip_prefix("afs://").unwrap_or(url);

        let backend_config = if let Ok(config_path) = std::env::var("AFS_BACKEND_CONFIG") {
            let content = std::fs::read_to_string(&config_path)
                .with_context(|| format!("read backend config: {}", config_path))?;
            let mut config: afs_store::backend::BackendConfig = toml::from_str(&content)?;
            if !path.is_empty() {
                config = with_prefix(config, path);
            }
            config
        } else {
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
            if config_file.is_none()
                && path.ends_with(".toml")
                && std::path::Path::new(path).exists()
            {
                config_file = Some(path.to_string());
                prefix = "";
            }

            match config_file {
                Some(cf) => {
                    let content = std::fs::read_to_string(&cf)?;
                    let mut config: afs_store::backend::BackendConfig =
                        toml::from_str(&content)?;
                    if !prefix.is_empty() {
                        config = with_prefix(config, prefix);
                    }
                    config
                }
                None => afs_store::backend::BackendConfig::Fs {
                    root: if path.is_empty() {
                        "/tmp/afs-remote".to_string()
                    } else {
                        path.to_string()
                    },
                },
            }
        };

        let op = afs_store::backend::create_operator(&backend_config)?;
        let git_dir = resolve_git_dir()?;

        Ok(Self { op, git_dir })
    }

    pub fn operator(&self) -> &Operator {
        &self.op
    }

    /// List refs on the remote.
    pub async fn list_refs(&self) -> Result<Vec<(String, String)>> {
        let remote_refs = refs::read_refs(&self.op).await?;

        // Extract HEAD target (stored as "HEAD" → "refs/heads/xxx" in refs map)
        let head_target = remote_refs.get("HEAD").cloned();

        let mut result: Vec<(String, String)> = remote_refs
            .into_iter()
            .filter(|(k, _)| k != "HEAD") // Don't list HEAD as a regular ref
            .collect();

        // Advertise HEAD symref if we know the default branch
        if let Some(target) = &head_target {
            if let Some((_refname, oid)) = result.iter().find(|(r, _)| r == target) {
                result.push((
                    format!("@{} HEAD", target),
                    oid.clone(),
                ));
            }
        }

        Ok(result)
    }

    /// Push a refspec with optimistic locking on refs.json.
    pub async fn push(&self, spec: &str) -> Result<()> {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        let (src, dst) = spec
            .split_once(':')
            .context("invalid push spec, expected src:dst")?;

        // Handle delete
        if src.is_empty() {
            info!(%dst, "deleting remote ref");
            let dst = dst.to_string();
            refs::update_refs(&self.op, |r| {
                r.remove(&dst);
            })
            .await?;
            return Ok(());
        }

        // Resolve local ref to OID
        let local_oid = resolve_ref(&self.git_dir, src)?;
        info!(%src, %dst, %local_oid, "pushing");

        // Generate pack + idx and upload to standard git layout
        let pack_result = generate_pack_files(&self.git_dir, &local_oid, &self.op).await?;

        if let Some((pack_hash, pack_data, idx_data)) = pack_result {
            let pack_key = format!("objects/pack/pack-{}.pack", pack_hash);
            let idx_key = format!("objects/pack/pack-{}.idx", pack_hash);
            info!(pack_key = %pack_key, size = pack_data.len(), "uploading pack + idx");
            self.op
                .write(&pack_key, pack_data)
                .await
                .context("upload git pack")?;
            self.op
                .write(&idx_key, idx_data)
                .await
                .context("upload git idx")?;

            // Update objects/info/packs listing
            update_info_packs(&self.op).await?;
        }

        // Update ref with optimistic lock
        let dst = dst.to_string();
        refs::update_refs(&self.op, |r| {
            // Set HEAD to point to the first branch pushed (if not already set)
            if dst.starts_with("refs/heads/") && !r.contains_key("HEAD") {
                r.insert("HEAD".to_string(), dst.clone());
            }
            r.insert(dst.clone(), local_oid.clone());
        })
        .await?;

        Ok(())
    }

    /// Fetch objects for a given oid + refname. Supports shallow depth.
    pub async fn fetch(&self, spec: &str) -> Result<()> {
        let parts: Vec<&str> = spec.splitn(2, ' ').collect();
        if parts.len() < 2 {
            anyhow::bail!("invalid fetch spec: {}", spec);
        }
        let oid = parts[0];
        let refname = parts[1];
        info!(%oid, %refname, "fetching");

        // Track which packs we already fetched
        let fetched_dir = self.git_dir.join("afs-fetched-packs");
        std::fs::create_dir_all(&fetched_dir).ok();

        let entries = self.op.list("objects/pack/").await.unwrap_or_default();
        for entry in entries {
            let key = entry.name();
            if !key.ends_with(".pack") {
                continue;
            }

            let marker = fetched_dir.join(key);
            if marker.exists() {
                debug!(%key, "already fetched, skipping");
                continue;
            }

            let full_key = format!("objects/pack/{}", key);
            debug!(%full_key, "downloading pack");
            let pack_data = self.op.read(&full_key).await?.to_vec();

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

            std::fs::write(&marker, "").ok();
        }

        Ok(())
    }

    /// Fetch with shallow depth — only download objects reachable within N commits.
    pub async fn fetch_shallow(&self, spec: &str, depth: u32) -> Result<()> {
        let parts: Vec<&str> = spec.splitn(2, ' ').collect();
        if parts.len() < 2 {
            anyhow::bail!("invalid fetch spec: {}", spec);
        }
        let oid = parts[0];
        let refname = parts[1];
        info!(%oid, %refname, depth, "shallow fetch");

        // Download all packs (same as regular fetch — packs already contain
        // only the objects that were pushed). The shallow boundary is enforced
        // locally after fetching by grafting.
        self.fetch(spec).await?;

        // Create shallow boundary: tell git this commit has no parents beyond depth
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["rev-list", "--max-count", &depth.to_string(), oid])
            .output()
            .context("git rev-list for shallow")?;

        if output.status.success() {
            let commits: Vec<&str> = std::str::from_utf8(&output.stdout)
                .unwrap_or("")
                .lines()
                .collect();

            if let Some(&boundary) = commits.last() {
                let shallow_file = self.git_dir.join("shallow");
                let mut shallow_oids: Vec<String> = if shallow_file.exists() {
                    std::fs::read_to_string(&shallow_file)?
                        .lines()
                        .map(|s| s.to_string())
                        .collect()
                } else {
                    Vec::new()
                };

                if !shallow_oids.contains(&boundary.to_string()) {
                    shallow_oids.push(boundary.to_string());
                    std::fs::write(&shallow_file, shallow_oids.join("\n") + "\n")?;
                    info!(%boundary, "set shallow boundary");
                }
            }
        }

        Ok(())
    }

    /// GC: repack all remote packs into one, delete old packs.
    pub async fn gc(&self) -> Result<GcStats> {
        info!("starting remote GC");

        let entries = self.op.list("objects/pack/").await.unwrap_or_default();
        let pack_keys: Vec<String> = entries
            .iter()
            .filter(|e| e.name().ends_with(".pack"))
            .map(|e| format!("objects/pack/{}", e.name()))
            .collect();

        if pack_keys.len() <= 1 {
            info!("only {} pack(s), nothing to repack", pack_keys.len());
            return Ok(GcStats {
                packs_before: pack_keys.len(),
                packs_after: pack_keys.len(),
                bytes_before: 0,
                bytes_after: 0,
            });
        }

        // First, ensure we have all packs fetched locally
        let fetched_dir = self.git_dir.join("afs-fetched-packs");
        std::fs::create_dir_all(&fetched_dir).ok();

        let mut total_before = 0u64;
        for key in &pack_keys {
            let data = self.op.read(key).await?.to_vec();
            total_before += data.len() as u64;

            let filename = key.rsplit('/').next().unwrap();
            let marker = fetched_dir.join(filename);
            if !marker.exists() {
                let tmp = std::env::temp_dir().join(format!("afs-gc-{}", filename));
                std::fs::write(&tmp, &data)?;

                let status = Command::new("git")
                    .arg("--git-dir")
                    .arg(&self.git_dir)
                    .args(["index-pack", "--stdin"])
                    .stdin(std::fs::File::open(&tmp)?)
                    .stdout(std::process::Stdio::null())
                    .status()?;
                let _ = std::fs::remove_file(&tmp);

                if status.success() {
                    std::fs::write(&marker, "").ok();
                }
            }
        }

        // Use remote refs as starting points for rev-list (skip HEAD pointer)
        let remote_refs = refs::read_refs(&self.op).await?;
        let ref_oids: Vec<String> = remote_refs
            .iter()
            .filter(|(k, _)| k.as_str() != "HEAD")
            .map(|(_, v)| v.clone())
            .collect();

        if ref_oids.is_empty() {
            return Ok(GcStats {
                packs_before: pack_keys.len(),
                packs_after: pack_keys.len(),
                bytes_before: total_before,
                bytes_after: total_before,
            });
        }

        let mut rev_args = vec!["--objects".to_string()];
        rev_args.extend(ref_oids);

        let rev_list = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("rev-list")
            .args(&rev_args)
            .output()
            .context("git rev-list for GC")?;

        if !rev_list.status.success() || rev_list.stdout.is_empty() {
            return Ok(GcStats {
                packs_before: pack_keys.len(),
                packs_after: pack_keys.len(),
                bytes_before: total_before,
                bytes_after: total_before,
            });
        }

        // Repack all objects into one pack
        let mut pack_cmd = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["pack-objects", "--stdout"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("git pack-objects for GC")?;

        let mut object_count = 0usize;
        {
            use std::io::Write;
            let stdin = pack_cmd.stdin.as_mut().unwrap();
            for line in String::from_utf8_lossy(&rev_list.stdout).lines() {
                let oid = line.split_whitespace().next().unwrap_or("");
                if !oid.is_empty() {
                    writeln!(stdin, "{}", oid)?;
                    object_count += 1;
                }
            }
        }

        let output = pack_cmd.wait_with_output()?;
        if !output.status.success() {
            anyhow::bail!("git pack-objects failed during GC");
        }

        let new_pack = output.stdout;

        // Write pack to temp file and index it to get the correct SHA-1 hash + .idx
        let tmp_pack = std::env::temp_dir().join("afs-gc-repack.pack");
        std::fs::write(&tmp_pack, &new_pack)?;

        let idx_output = Command::new("git")
            .args(["index-pack", tmp_pack.to_str().unwrap()])
            .output()
            .context("git index-pack for GC repack")?;

        let pack_sha1 = String::from_utf8_lossy(&idx_output.stdout).trim().to_string();
        let tmp_idx = tmp_pack.with_extension("idx");
        let idx_data = std::fs::read(&tmp_idx).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp_pack);
        let _ = std::fs::remove_file(&tmp_idx);

        let new_pack_key = format!("objects/pack/pack-{}.pack", pack_sha1);
        let new_idx_key = format!("objects/pack/pack-{}.idx", pack_sha1);

        info!(
            objects = object_count,
            old_packs = pack_keys.len(),
            old_size = total_before,
            new_size = new_pack.len(),
            "repacked"
        );

        self.op
            .write(&new_pack_key, new_pack.clone())
            .await
            .context("upload repacked pack")?;
        self.op
            .write(&new_idx_key, idx_data)
            .await
            .context("upload repacked idx")?;

        // Delete old packs + their idx files
        for key in &pack_keys {
            if *key != new_pack_key {
                debug!(%key, "deleting old pack");
                self.op.delete(key).await.ok();
                // Also delete corresponding .idx
                let idx_key = key.replace(".pack", ".idx");
                self.op.delete(&idx_key).await.ok();
            }
        }

        // Update objects/info/packs
        update_info_packs(&self.op).await?;

        Ok(GcStats {
            packs_before: pack_keys.len(),
            packs_after: 1,
            bytes_before: total_before,
            bytes_after: new_pack.len() as u64,
        })
    }
}

pub struct GcStats {
    pub packs_before: usize,
    pub packs_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

// ── Helpers ────────────────────────────────────────────────────

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

/// Generate pack + idx files. Returns (sha1_hash, pack_bytes, idx_bytes) or None if nothing to pack.
async fn generate_pack_files(
    git_dir: &PathBuf,
    oid: &str,
    op: &Operator,
) -> Result<Option<(String, Vec<u8>, Vec<u8>)>> {
    let remote_refs = refs::read_refs(op).await?;

    let mut rev_list_args = vec!["--objects".to_string(), oid.to_string()];
    for (refname, oid_val) in &remote_refs {
        // Skip HEAD pointer (it stores a refname, not an OID)
        if refname == "HEAD" {
            continue;
        }
        rev_list_args.push(format!("^{}", oid_val));
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
        return Ok(None);
    }

    // Write pack to a temp file so git gives us the SHA-1 hash + .idx
    let tmp_dir = tempfile::tempdir().context("create temp dir for pack")?;
    let pack_base = tmp_dir.path().join("pack");

    let mut pack_objects = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["pack-objects", pack_base.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("git pack-objects")?;

    let object_count;
    {
        use std::io::Write;
        let stdin = pack_objects.stdin.as_mut().unwrap();
        let mut count = 0usize;
        for line in String::from_utf8_lossy(&rev_list.stdout).lines() {
            let obj_oid = line.split_whitespace().next().unwrap_or("");
            if !obj_oid.is_empty() {
                writeln!(stdin, "{}", obj_oid)?;
                count += 1;
            }
        }
        object_count = count;
    }

    let output = pack_objects.wait_with_output().context("pack-objects")?;
    if !output.status.success() {
        anyhow::bail!("git pack-objects failed");
    }

    // git pack-objects outputs the SHA-1 hash to stdout
    let pack_sha1 = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let pack_path = tmp_dir.path().join(format!("pack-{}.pack", pack_sha1));
    let idx_path = tmp_dir.path().join(format!("pack-{}.idx", pack_sha1));

    let pack_data = std::fs::read(&pack_path).context("read generated pack")?;
    let idx_data = std::fs::read(&idx_path).context("read generated idx")?;

    info!(objects = object_count, pack_size = pack_data.len(), %pack_sha1, "packed objects");

    Ok(Some((pack_sha1, pack_data, idx_data)))
}

/// Update the objects/info/packs file listing all pack files on the remote.
async fn update_info_packs(op: &Operator) -> Result<()> {
    let entries = op.list("objects/pack/").await.unwrap_or_default();
    let mut content = String::new();
    for entry in &entries {
        let name = entry.name();
        if name.ends_with(".pack") {
            content.push_str("P ");
            content.push_str(name);
            content.push('\n');
        }
    }
    op.write("objects/info/packs", content.into_bytes())
        .await
        .context("write objects/info/packs")?;
    Ok(())
}

fn with_prefix(
    config: afs_store::backend::BackendConfig,
    prefix: &str,
) -> afs_store::backend::BackendConfig {
    match config {
        afs_store::backend::BackendConfig::S3 {
            bucket,
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            prefix: existing,
        } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::S3 {
                bucket,
                region,
                endpoint,
                access_key_id,
                secret_access_key,
                prefix: new_prefix,
            }
        }
        afs_store::backend::BackendConfig::Gcs {
            bucket,
            credential,
            prefix: existing,
        } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::Gcs {
                bucket,
                credential,
                prefix: new_prefix,
            }
        }
        afs_store::backend::BackendConfig::AzBlob {
            container,
            account_name,
            account_key,
            prefix: existing,
        } => {
            let new_prefix = match existing {
                Some(p) => Some(format!("{}/{}", p.trim_end_matches('/'), prefix)),
                None => Some(prefix.to_string()),
            };
            afs_store::backend::BackendConfig::AzBlob {
                container,
                account_name,
                account_key,
                prefix: new_prefix,
            }
        }
        afs_store::backend::BackendConfig::Fs { root } => afs_store::backend::BackendConfig::Fs {
            root: format!("{}/{}", root.trim_end_matches('/'), prefix),
        },
    }
}

