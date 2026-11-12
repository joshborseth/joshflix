//! `android/version.toml`: the single source of the app version.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::util::workspace_root;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub version_name: String,
    pub version_code: u64,
}

impl Version {
    pub fn load() -> Result<Self> {
        let path = workspace_root().join("android/version.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let version: Version =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        ensure!(
            !version.version_name.is_empty()
                && version
                    .version_name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
            "version_name must be non-empty and use only [A-Za-z0-9.-] (it goes into file names)"
        );
        // Android caps versionCode at 2100000000.
        ensure!(
            (1..=2_100_000_000).contains(&version.version_code),
            "version_code must be between 1 and 2100000000"
        );
        Ok(version)
    }
}
