//! On-disk credential for the CLI (`~/.config/nex/config.json`, mode 0600).
//! Apps should keep the credential in the platform keychain instead.

use anyhow::{Context, Result};
use nex_proto::DeviceCredential;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredConfig {
    pub server: String,
    pub credential: DeviceCredential,
}

pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NEX_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("nex").join("config.json")
}

pub fn load(path: &Path) -> Result<StoredConfig> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "no nex credential at {} (run `nex init` or `nex join` first)",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
}

pub fn save(path: &Path, config: &StoredConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(serde_json::to_string_pretty(config)?.as_bytes())?;
    Ok(())
}
