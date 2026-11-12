//! App updates from the private relay: check, download, verify.
//!
//! Installing the verified APK is platform work and lives in the app crate.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use joshflix_protocol::{UPDATE_MANIFEST_PATH, UpdateApk, UpdateManifest};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::error::{CoreError, Result};
use crate::http::Http;

const SERVICE: &str = "the Joshflix update server";

/// An update newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub manifest: UpdateManifest,
    pub apk: UpdateApk,
}

#[derive(Clone)]
pub struct Updater {
    http: Http,
    base: String,
}

impl Updater {
    pub fn new(http: Http, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    /// `Ok(None)` when nothing newer is published for these ABIs.
    pub async fn check(&self, current_code: u64, abis: &[&str]) -> Result<Option<AvailableUpdate>> {
        let response = self
            .http
            .client()
            .get(format!("{}{UPDATE_MANIFEST_PATH}", self.base))
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let manifest: UpdateManifest = crate::pairing::read(response).await?;
        if manifest.version_code <= current_code {
            return Ok(None);
        }
        Ok(manifest.apk_for(abis).cloned().map(|apk| AvailableUpdate { manifest, apk }))
    }

    /// Download the APK into `dir`, verifying size and SHA-256. Reports 0.0–1.0.
    pub async fn download(
        &self,
        update: &AvailableUpdate,
        dir: &Path,
        progress: impl Fn(f32),
        cancel: watch::Receiver<bool>,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(dir).await?;
        let file_name = update
            .apk
            .path
            .rsplit('/')
            .next()
            .filter(|n| n.ends_with(".apk") && !n.contains(".."))
            .ok_or_else(|| CoreError::Invalid("Bad update file name.".into()))?;
        let target = dir.join(file_name);
        let partial = dir.join(format!("{file_name}.part"));
        let response = self
            .http
            .client()
            .get(format!("{}{}", self.base, update.apk.path))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        if !response.status().is_success() {
            return Err(CoreError::Status { service: SERVICE, status: response.status().as_u16() });
        }
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let cancelled = *cancel.borrow();
            if cancelled {
                drop(file);
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(CoreError::Cancelled);
            }
            let chunk = chunk.map_err(|source| CoreError::Network { service: SERVICE, source })?;
            received += chunk.len() as u64;
            if received > update.apk.size {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(CoreError::Invalid("The update is larger than announced.".into()));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            progress(received as f32 / update.apk.size.max(1) as f32);
        }
        file.flush().await?;
        drop(file);
        let digest = hex(&hasher.finalize());
        if received != update.apk.size || !digest.eq_ignore_ascii_case(&update.apk.sha256) {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(CoreError::Invalid(
                "The downloaded update didn't match its checksum. Try again.".into(),
            ));
        }
        tokio::fs::rename(&partial, &target).await?;
        Ok(target)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
