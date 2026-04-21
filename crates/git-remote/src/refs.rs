//! Remote refs stored as a simple JSON file in S3.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use opendal::Operator;

const REFS_KEY: &str = "refs.json";

pub type RefMap = BTreeMap<String, String>;

/// Read refs from remote storage.
pub async fn read_refs(op: &Operator) -> Result<RefMap> {
    match op.read(REFS_KEY).await {
        Ok(buf) => {
            let refs: RefMap =
                serde_json::from_slice(&buf.to_bytes()).context("parse refs.json")?;
            Ok(refs)
        }
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(RefMap::new()),
        Err(e) => Err(e).context("read refs.json"),
    }
}

/// Write refs to remote storage.
pub async fn write_refs(op: &Operator, refs: &RefMap) -> Result<()> {
    let data = serde_json::to_vec_pretty(refs)?;
    op.write(REFS_KEY, data).await.context("write refs.json")?;
    Ok(())
}
