//! Git LFS object fetch — S3 direct + LFS batch API fallback.
//!
//! LFS objects are stored individually at `lfs/{oid[0:2]}/{oid}` in S3.
//! If not found in S3, we fall back to the LFS batch API to locate them
//! on the original LFS server.

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use opendal::Operator;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::backend;
use crate::cache::BlobCache;

/// Fetch an LFS object by its SHA-256 OID.
/// Tries: cache → S3 lfs/ prefix → LFS batch API fallback.
pub async fn fetch_lfs_object(
    oid: &str,
    cache: &BlobCache,
    operator: &Operator,
    lfs_server_url: Option<&str>,
    client: &reqwest::Client,
) -> Result<Bytes> {
    // Check cache first
    if let Some(data) = cache.get(oid).await {
        debug!(%oid, "LFS cache hit");
        return Ok(data);
    }

    // Try S3 lfs/ prefix
    let key = backend::lfs_key(oid);
    match operator.read(&key).await {
        Ok(buf) => {
            let data = buf.to_bytes();
            debug!(%oid, size = data.len(), "LFS object fetched from S3");
            cache.insert(oid.to_string(), data.clone());
            return Ok(data);
        }
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
            debug!(%oid, "LFS object not in S3, trying batch API");
        }
        Err(e) => return Err(e).context(format!("fetch LFS object {} from S3", oid)),
    }

    // Fallback: LFS batch API
    let server_url = lfs_server_url.context(
        "LFS object not in S3 and no LFS server URL configured",
    )?;

    let data = fetch_via_batch_api(server_url, oid, client).await?;
    let bytes = Bytes::from(data);
    cache.insert(oid.to_string(), bytes.clone());

    Ok(bytes)
}

/// Fetch multiple LFS objects in a single batch API request, downloading concurrently.
/// Returns (oid, data) pairs for all successfully fetched objects.
pub async fn fetch_lfs_objects_batch(
    oids: &[&str],
    cache: &BlobCache,
    operator: &Operator,
    lfs_server_url: Option<&str>,
    client: &reqwest::Client,
) -> Result<Vec<(String, Bytes)>> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::with_capacity(oids.len());
    let mut need_fetch: Vec<&str> = Vec::new();

    // Check cache and S3 first
    for &oid in oids {
        if let Some(data) = cache.get(oid).await {
            debug!(%oid, "LFS batch: cache hit");
            results.push((oid.to_string(), data));
            continue;
        }

        let key = backend::lfs_key(oid);
        match operator.read(&key).await {
            Ok(buf) => {
                let data = buf.to_bytes();
                debug!(%oid, "LFS batch: S3 hit");
                cache.insert(oid.to_string(), data.clone());
                results.push((oid.to_string(), data));
            }
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                need_fetch.push(oid);
            }
            Err(e) => return Err(e).context(format!("fetch LFS object {} from S3", oid)),
        }
    }

    if need_fetch.is_empty() {
        return Ok(results);
    }

    let server_url = match lfs_server_url {
        Some(url) => url,
        None => {
            warn!(
                count = need_fetch.len(),
                "LFS objects not in S3 and no LFS server URL configured"
            );
            return Ok(results);
        }
    };

    // Send all remaining OIDs in one batch request
    let batch_url = format!("{}/objects/batch", server_url.trim_end_matches('/'));

    let request = BatchRequest {
        operation: "download",
        transfers: vec!["basic"],
        objects: need_fetch
            .iter()
            .map(|oid| BatchObject {
                oid: oid.to_string(),
                size: 0,
            })
            .collect(),
    };

    let response = client
        .post(&batch_url)
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(&request)
        .send()
        .await
        .context("LFS batch API request")?;

    if !response.status().is_success() {
        anyhow::bail!("LFS batch API returned status {}", response.status());
    }

    let batch: BatchResponse = response.json().await.context("parse LFS batch response")?;

    // Download all objects concurrently (limit to 8)
    let downloads: Vec<_> = batch
        .objects
        .into_iter()
        .filter_map(|obj| {
            if let Some(err) = &obj.error {
                warn!(oid = %obj.oid, code = err.code, msg = %err.message, "LFS batch error for object");
                return None;
            }
            let actions = obj.actions.as_ref()?;
            let download = actions.download.as_ref()?;
            Some((obj.oid.clone(), download.href.clone(), download.header.clone()))
        })
        .collect();

    let fetched: Vec<_> = stream::iter(downloads)
        .map(|(oid, href, headers)| {
            let client = client.clone();
            async move {
                let mut req = client.get(&href);
                if let Some(hdrs) = &headers {
                    for (k, v) in hdrs {
                        req = req.header(k, v);
                    }
                }
                match req.send().await {
                    Ok(resp) => match resp.bytes().await {
                        Ok(data) => Some((oid, Bytes::from(data))),
                        Err(e) => {
                            warn!(%oid, error = %e, "LFS download body failed");
                            None
                        }
                    },
                    Err(e) => {
                        warn!(%oid, error = %e, "LFS download request failed");
                        None
                    }
                }
            }
        })
        .buffer_unordered(8)
        .collect()
        .await;

    for item in fetched.into_iter().flatten() {
        cache.insert(item.0.clone(), item.1.clone());
        results.push(item);
    }

    Ok(results)
}

/// Upload an LFS object to S3.
pub async fn upload_lfs_object(
    oid: &str,
    data: &[u8],
    operator: &Operator,
) -> Result<()> {
    let key = backend::lfs_key(oid);
    operator
        .write(&key, data.to_vec())
        .await
        .context("upload LFS object to S3")?;
    info!(%oid, size = data.len(), "LFS object uploaded to S3");
    Ok(())
}

// ── LFS Batch API ──────────────────────────────────────────────

/// Request body for LFS batch API.
#[derive(Serialize)]
struct BatchRequest {
    operation: &'static str,
    transfers: Vec<&'static str>,
    objects: Vec<BatchObject>,
}

#[derive(Serialize)]
struct BatchObject {
    oid: String,
    size: u64,
}

/// Response from LFS batch API.
#[derive(Deserialize)]
struct BatchResponse {
    objects: Vec<BatchResponseObject>,
}

#[derive(Deserialize)]
struct BatchResponseObject {
    oid: String,
    #[allow(dead_code)]
    size: u64,
    actions: Option<BatchActions>,
    error: Option<BatchError>,
}

#[derive(Deserialize)]
struct BatchActions {
    download: Option<BatchAction>,
}

#[derive(Deserialize)]
struct BatchAction {
    href: String,
    header: Option<std::collections::HashMap<String, String>>,
}

#[derive(Deserialize)]
struct BatchError {
    code: i32,
    message: String,
}

/// Fetch an LFS object via the batch API.
/// See: https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md
async fn fetch_via_batch_api(
    server_url: &str,
    oid: &str,
    client: &reqwest::Client,
) -> Result<Vec<u8>> {
    let batch_url = format!("{}/objects/batch", server_url.trim_end_matches('/'));

    let request = BatchRequest {
        operation: "download",
        transfers: vec!["basic"],
        objects: vec![BatchObject {
            oid: oid.to_string(),
            size: 0, // Size unknown, server will respond with it
        }],
    };

    let response = client
        .post(&batch_url)
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(&request)
        .send()
        .await
        .context("LFS batch API request")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "LFS batch API returned status {}",
            response.status()
        );
    }

    let batch: BatchResponse = response.json().await.context("parse LFS batch response")?;

    let obj = batch
        .objects
        .into_iter()
        .find(|o| o.oid == oid)
        .context("LFS object not found in batch response")?;

    if let Some(err) = obj.error {
        anyhow::bail!("LFS batch error {}: {}", err.code, err.message);
    }

    let actions = obj.actions.context("no download actions in LFS response")?;
    let download = actions.download.context("no download action in LFS response")?;

    info!(%oid, url = %download.href, "downloading LFS object via batch API");

    let mut req = client.get(&download.href);
    if let Some(headers) = &download.header {
        for (k, v) in headers {
            req = req.header(k, v);
        }
    }

    let data = req
        .send()
        .await
        .context("LFS download request")?
        .bytes()
        .await
        .context("LFS download body")?;

    Ok(data.to_vec())
}
