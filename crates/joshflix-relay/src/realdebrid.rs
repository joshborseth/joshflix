//! The one Real-Debrid call the relay makes: `GET /user` to check a key.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;

/// Why a key could not be confirmed.
#[derive(Debug)]
pub enum RdError {
    /// Real-Debrid answered 401 or 403.
    InvalidKey,
    /// Network failure, timeout, 5xx, or an unexpected response.
    Unreachable,
}

#[derive(Clone)]
pub struct RealDebrid {
    client: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct User {
    username: String,
}

impl RealDebrid {
    pub fn new(base: &str, timeout: Duration) -> Result<Self, reqwest::Error> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        // The ring provider always supports the default protocol versions.
        .expect("ring supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("joshflix-relay/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// Returns the Real-Debrid username for `api_key`.
    pub async fn verify(&self, api_key: &str) -> Result<String, RdError> {
        let response = self
            .client
            .get(format!("{}/user", self.base))
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(error = %error.without_url(), "Real-Debrid request failed");
                RdError::Unreachable
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(RdError::InvalidKey);
        }
        if !status.is_success() {
            tracing::warn!(%status, "Real-Debrid returned an error status");
            return Err(RdError::Unreachable);
        }
        let user: User = response.json().await.map_err(|error| {
            tracing::warn!(error = %error.without_url(), "Real-Debrid returned an unexpected body");
            RdError::Unreachable
        })?;
        Ok(user.username)
    }
}

/// Cheap shape check before spending a Real-Debrid call: 20–100 ASCII
/// letters and digits.
pub fn key_looks_valid(key: &str) -> bool {
    (20..=100).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape() {
        assert!(key_looks_valid("ABCDEFGHIJKLMNOPQRST"));
        assert!(key_looks_valid(&"a1".repeat(26)));
        assert!(!key_looks_valid("short"));
        assert!(!key_looks_valid(&"a".repeat(101)));
        assert!(!key_looks_valid("ABCDEFGHIJKLMNOPQRS-"));
        assert!(!key_looks_valid("ABCDEFGHIJ KLMNOPQRST"));
        assert!(!key_looks_valid("ÄBCDEFGHIJKLMNOPQRST"));
    }
}
