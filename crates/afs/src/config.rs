use std::path::PathBuf;

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Root directory for AFS runtime data.
    #[serde(default = "default_data_root")]
    pub data_root: PathBuf,

    /// Pack file configuration.
    #[serde(default)]
    pub pack: afs_store::pack::PackConfig,

    /// Cache configuration.
    #[serde(default)]
    pub cache: afs_store::cache::CacheConfig,
}

fn default_data_root() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join("afs"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/afs"))
}

impl Config {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let config_path = std::env::var("AFS_CONFIG").ok().map(PathBuf::from).unwrap_or_else(|| {
            dirs::config_dir()
                .map(|d| d.join("afs/config.toml"))
                .unwrap_or_else(|| PathBuf::from("/etc/afs/config.toml"))
        });

        Figment::new()
            .merge(Toml::file(&config_path))
            .merge(Env::prefixed("AFS_"))
            .extract()
            .map_err(Box::new)
    }
}
