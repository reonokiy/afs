//! Remote refs stored as a simple JSON file in S3.
//! Uses optimistic locking via ETag to prevent concurrent push data loss.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use opendal::Operator;

const REFS_KEY: &str = "refs.json";
const MAX_RETRIES: usize = 5;

pub type RefMap = BTreeMap<String, String>;

/// Refs snapshot with its ETag for conditional writes.
pub struct RefsSnapshot {
    pub refs: RefMap,
    pub etag: Option<String>,
}

/// Read refs from remote storage, returning the snapshot with ETag.
pub async fn read_refs_snapshot(op: &Operator) -> Result<RefsSnapshot> {
    match op.stat(REFS_KEY).await {
        Ok(meta) => {
            let etag = meta.etag().map(|s| s.to_string());
            let buf = op.read(REFS_KEY).await.context("read refs.json")?;
            let refs: RefMap =
                serde_json::from_slice(&buf.to_bytes()).context("parse refs.json")?;
            Ok(RefsSnapshot { refs, etag })
        }
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(RefsSnapshot {
            refs: RefMap::new(),
            etag: None,
        }),
        Err(e) => Err(e).context("stat refs.json"),
    }
}

/// Read refs (convenience wrapper without ETag).
pub async fn read_refs(op: &Operator) -> Result<RefMap> {
    Ok(read_refs_snapshot(op).await?.refs)
}

/// Write refs with optimistic locking.
/// If the backend supports ETags (S3/GCS/Azure), uses conditional write.
/// Falls back to unconditional write for backends without ETag support (FS).
pub async fn write_refs(op: &Operator, refs: &RefMap, expected_etag: Option<&str>) -> Result<()> {
    let data = serde_json::to_vec_pretty(refs)?;

    if let Some(etag) = expected_etag {
        // Try conditional write — fails if someone else modified refs.json
        match op
            .write_with(REFS_KEY, data.clone())
            .if_match(etag)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == opendal::ErrorKind::ConditionNotMatch => {
                anyhow::bail!(
                    "refs.json was modified by another push (ETag mismatch). Please pull and retry."
                );
            }
            Err(e) if e.kind() == opendal::ErrorKind::Unsupported => {
                // Backend doesn't support conditional writes, fall through
            }
            Err(e) => return Err(e).context("conditional write refs.json"),
        }
    }

    // Unconditional write (FS backend or no ETag available)
    op.write(REFS_KEY, data).await.context("write refs.json")?;

    // Also write git-compatible files for dumb HTTP protocol
    write_git_compat(op, refs).await?;

    Ok(())
}

/// Write git dumb-HTTP-compatible ref files:
///   HEAD         — "ref: refs/heads/main\n"
///   info/refs    — "<oid>\t<refname>\n" for each ref
async fn write_git_compat(op: &Operator, refs: &RefMap) -> Result<()> {
    // info/refs — tab-separated oid + refname (exclude the HEAD pointer entry)
    let mut info_refs = String::new();
    for (refname, oid) in refs {
        if refname == "HEAD" {
            continue; // HEAD is a symref, handled separately
        }
        info_refs.push_str(oid);
        info_refs.push('\t');
        info_refs.push_str(refname);
        info_refs.push('\n');
    }
    op.write("info/refs", info_refs.into_bytes())
        .await
        .context("write info/refs")?;

    // HEAD — use the explicitly stored HEAD target, or fall back to first branch
    let default_branch = refs
        .get("HEAD")
        .cloned()
        .or_else(|| {
            refs.keys()
                .find(|k| k.starts_with("refs/heads/"))
                .cloned()
        });

    if let Some(branch) = default_branch {
        let head = format!("ref: {}\n", branch);
        op.write("HEAD", head.into_bytes())
            .await
            .context("write HEAD")?;
    }

    Ok(())
}

/// Update refs atomically: read, apply mutation, write with lock.
/// Retries on conflict up to MAX_RETRIES times.
pub async fn update_refs<F>(op: &Operator, mut mutate: F) -> Result<()>
where
    F: FnMut(&mut RefMap),
{
    for attempt in 0..MAX_RETRIES {
        let snapshot = read_refs_snapshot(op).await?;
        let mut refs = snapshot.refs;
        mutate(&mut refs);

        match write_refs(op, &refs, snapshot.etag.as_deref()).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_RETRIES - 1 => {
                tracing::warn!(attempt, error = %e, "refs update conflict, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt as u64 + 1)))
                    .await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
