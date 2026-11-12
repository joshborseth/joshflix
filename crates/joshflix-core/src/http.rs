//! The shared HTTP client.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::{CoreError, Result};

/// User agent sent to every service. Deliberately generic.
pub const USER_AGENT: &str = concat!("Joshflix/", env!("CARGO_PKG_VERSION"));

/// Cheap-to-clone HTTP client.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    /// Same settings but never follows redirects (to read Comet's 302 targets).
    no_redirect: reqwest::Client,
}

impl Http {
    pub fn new() -> Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| CoreError::Invalid(format!("TLS setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let builder = |tls: rustls::ClientConfig| {
            reqwest::Client::builder()
                .tls_backend_preconfigured(tls)
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(25))
                .pool_idle_timeout(Duration::from_secs(60))
        };
        let client = builder(tls.clone())
            .build()
            .map_err(|e| CoreError::Invalid(format!("HTTP client setup failed: {e}")))?;
        let no_redirect = builder(tls)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| CoreError::Invalid(format!("HTTP client setup failed: {e}")))?;
        Ok(Self { client, no_redirect })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn no_redirect(&self) -> &reqwest::Client {
        &self.no_redirect
    }

    /// GET a URL and decode JSON, mapping failures to [`CoreError`].
    pub async fn get_json<T: DeserializeOwned>(&self, service: &'static str, url: &str) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|source| CoreError::Network { service, source })?;
        decode_json(service, response).await
    }

    /// GET a URL and return the body bytes.
    pub async fn get_bytes(&self, service: &'static str, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|source| CoreError::Network { service, source })?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::Status {
                service,
                status: status.as_u16(),
            });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|source| CoreError::Network { service, source })?;
        Ok(bytes.to_vec())
    }
}

pub(crate) async fn decode_json<T: DeserializeOwned>(
    service: &'static str,
    response: reqwest::Response,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::Status {
            service,
            status: status.as_u16(),
        });
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|source| CoreError::Network { service, source })?;
    serde_json::from_slice(&bytes).map_err(|e| CoreError::decode(service, e))
}
