use anyhow::Result;
use opendal::Operator;
use serde::Deserialize;

/// Storage backend configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum BackendConfig {
    /// S3-compatible storage (AWS S3, MinIO, R2, etc.)
    #[serde(rename = "s3")]
    S3 {
        bucket: String,
        region: Option<String>,
        endpoint: Option<String>,
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
        /// Prefix for all keys (e.g. "repo-id/")
        prefix: Option<String>,
    },
    /// Google Cloud Storage
    #[serde(rename = "gcs")]
    Gcs {
        bucket: String,
        credential: Option<String>,
        prefix: Option<String>,
    },
    /// Azure Blob Storage
    #[serde(rename = "azblob")]
    AzBlob {
        container: String,
        account_name: Option<String>,
        account_key: Option<String>,
        prefix: Option<String>,
    },
    /// Local filesystem (for testing / development)
    #[serde(rename = "fs")]
    Fs {
        root: String,
    },
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self::Fs {
            root: "/tmp/afs-store".to_string(),
        }
    }
}

/// Create an opendal Operator from the backend config.
pub fn create_operator(config: &BackendConfig) -> Result<Operator> {
    match config {
        BackendConfig::S3 {
            bucket,
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            prefix,
        } => {
            let mut builder = opendal::services::S3::default()
                .bucket(bucket);

            if let Some(r) = region {
                builder = builder.region(r);
            }
            if let Some(e) = endpoint {
                builder = builder.endpoint(e);
            }
            if let Some(k) = access_key_id {
                builder = builder.access_key_id(k);
            }
            if let Some(s) = secret_access_key {
                builder = builder.secret_access_key(s);
            }
            if let Some(p) = prefix {
                builder = builder.root(p);
            }

            let op = Operator::new(builder)?
                .finish();
            Ok(op)
        }
        BackendConfig::Gcs {
            bucket,
            credential,
            prefix,
        } => {
            let mut builder = opendal::services::Gcs::default()
                .bucket(bucket);

            if let Some(c) = credential {
                builder = builder.credential(c);
            }
            if let Some(p) = prefix {
                builder = builder.root(p);
            }

            let op = Operator::new(builder)?
                .finish();
            Ok(op)
        }
        BackendConfig::AzBlob {
            container,
            account_name,
            account_key,
            prefix,
        } => {
            let mut builder = opendal::services::Azblob::default()
                .container(container);

            if let Some(n) = account_name {
                builder = builder.account_name(n);
            }
            if let Some(k) = account_key {
                builder = builder.account_key(k);
            }
            if let Some(p) = prefix {
                builder = builder.root(p);
            }

            let op = Operator::new(builder)?
                .finish();
            Ok(op)
        }
        BackendConfig::Fs { root } => {
            let builder = opendal::services::Fs::default()
                .root(root);

            let op = Operator::new(builder)?
                .finish();
            Ok(op)
        }
    }
}

/// S3 key helpers — build paths for different blob types.
pub fn pack_key(pack_id: &str) -> String {
    format!("packs/{}.pack", pack_id)
}

pub fn blob_key(oid: &str) -> String {
    format!("blobs/{}/{}", &oid[..2], oid)
}

pub fn lfs_key(oid: &str) -> String {
    format!("lfs/{}/{}", &oid[..2], oid)
}

pub const MANIFEST_KEY: &str = "manifest.json";
