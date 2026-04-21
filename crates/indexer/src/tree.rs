use std::path::Path;

use afs_db::{BaseNode, NodeKind};
use anyhow::{Context, Result};
use tracing::info;

/// Clone a repository with `--filter=blob:none --no-checkout`.
/// Uses git CLI for reliability (gix clone API is complex).
pub fn blobless_clone(remote_url: &str, branch: &str, gitdir: &Path) -> Result<()> {
    if gitdir.exists() {
        info!(?gitdir, "gitdir already exists, skipping clone");
        return Ok(());
    }

    let parent = gitdir.parent().context("gitdir has no parent")?;
    std::fs::create_dir_all(parent)?;

    let tmp = tempfile::TempDir::new_in(parent)?;
    let tmp_path = tmp.path();

    info!(%remote_url, %branch, ?tmp_path, "starting blobless clone");

    let status = std::process::Command::new("git")
        .args([
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            "--single-branch",
            "--branch",
            branch,
            remote_url,
            &tmp_path.display().to_string(),
        ])
        .status()
        .context("spawn git clone")?;

    if !status.success() {
        anyhow::bail!("git clone failed with {}", status);
    }

    // Move the .git directory to the target gitdir
    let dot_git = tmp_path.join(".git");
    std::fs::rename(&dot_git, gitdir)
        .with_context(|| format!("rename {:?} -> {:?}", dot_git, gitdir))?;

    let _ = tmp.close();

    // Populate the index so git status works inside the mount
    read_tree_head(gitdir)?;

    info!(?gitdir, "blobless clone complete");
    Ok(())
}

/// Walk the git tree at HEAD and produce a flat list of BaseNode entries.
pub fn build_tree_index(gitdir: &Path, generation: i64) -> Result<Vec<BaseNode>> {
    let repo = gix::open(gitdir).context("open gitdir")?;
    let head = repo.head_commit().context("resolve HEAD commit")?;
    let tree = head.tree().context("get HEAD tree")?;

    let mut nodes = Vec::new();
    // Add root directory
    nodes.push(BaseNode {
        generation,
        path: ".".to_string(),
        parent: String::new(),
        kind: NodeKind::Dir,
        oid: None,
        mode: 0o40755,
        size: None,
    });

    collect_tree_entries(&repo, &tree, ".", generation, &mut nodes)?;

    info!(generation, count = nodes.len(), "tree index built");
    Ok(nodes)
}

fn collect_tree_entries(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    parent_path: &str,
    generation: i64,
    nodes: &mut Vec<BaseNode>,
) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry?;
        let name = std::str::from_utf8(entry.filename()).context("non-utf8 filename")?;

        let path = if parent_path == "." {
            name.to_string()
        } else {
            format!("{}/{}", parent_path, name)
        };

        let mode = entry.mode().value() as i64;
        let oid_hex = entry.oid().to_hex().to_string();

        match entry.mode().kind() {
            gix::object::tree::EntryKind::Tree => {
                nodes.push(BaseNode {
                    generation,
                    path: path.clone(),
                    parent: parent_path.to_string(),
                    kind: NodeKind::Dir,
                    oid: None,
                    mode,
                    size: None,
                });

                let subtree_obj = repo.find_object(entry.oid())?;
                let subtree = subtree_obj.into_tree();
                collect_tree_entries(repo, &subtree, &path, generation, nodes)?;
            }
            gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => {
                let header = repo.find_header(entry.oid()).ok();
                let blob_size = header.as_ref().map(|h| h.size() as i64);

                // Check for LFS pointer: small blobs might be LFS pointers
                let (kind, oid, size) = if blob_size.is_some_and(|s| s < crate::lfs_scan::LFS_POINTER_MAX_SIZE as i64) {
                    // Try to read the blob to check for LFS pointer
                    match repo.find_object(entry.oid()) {
                        Ok(obj) if crate::lfs_scan::might_be_lfs_pointer(&obj.data) => {
                            match crate::lfs_scan::parse_lfs_pointer(&obj.data) {
                                Some(ptr) => {
                                    // It's an LFS pointer — store LFS SHA-256 OID and real size
                                    (NodeKind::Lfs, Some(ptr.oid), Some(ptr.size as i64))
                                }
                                None => (NodeKind::Blob, Some(oid_hex), blob_size),
                            }
                        }
                        _ => (NodeKind::Blob, Some(oid_hex), blob_size),
                    }
                } else {
                    (NodeKind::Blob, Some(oid_hex), blob_size)
                };

                nodes.push(BaseNode {
                    generation,
                    path: path.clone(),
                    parent: parent_path.to_string(),
                    kind,
                    oid,
                    mode,
                    size,
                });
            }
            gix::object::tree::EntryKind::Link => {
                nodes.push(BaseNode {
                    generation,
                    path: path.clone(),
                    parent: parent_path.to_string(),
                    kind: NodeKind::Symlink,
                    oid: Some(oid_hex),
                    mode,
                    size: None,
                });
            }
            gix::object::tree::EntryKind::Commit => {
                // Submodule reference, skip
            }
        }
    }

    Ok(())
}

/// Resolve the current HEAD OID and ref name.
pub fn resolve_head(gitdir: &Path) -> Result<(String, String)> {
    let repo = gix::open(gitdir).context("open gitdir")?;
    let head = repo.head_ref().context("resolve HEAD ref")?;

    let (oid, refname) = match head {
        Some(reference) => {
            let oid = reference.id().to_hex().to_string();
            let name = reference.name().as_bstr().to_string();
            (oid, name)
        }
        None => {
            let commit = repo.head_commit().context("resolve HEAD commit")?;
            let oid = commit.id().to_hex().to_string();
            (oid, "HEAD".to_string())
        }
    };

    Ok((oid, refname))
}

/// Run `git read-tree HEAD` to populate the index.
pub fn read_tree_head(gitdir: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["read-tree", "HEAD"])
        .env("GIT_DIR", gitdir)
        .status()
        .context("spawn git read-tree")?;

    if !status.success() {
        anyhow::bail!("git read-tree HEAD failed with {}", status);
    }
    Ok(())
}
