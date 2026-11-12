//! TV side of the Real-Debrid QR pairing with the private relay.

use std::time::Duration;

use joshflix_protocol::{
    ApiError, CreatePairingResponse, MAX_POLL_WAIT_SECS, PAIRINGS_PATH, PairingPoll,
};
use tokio::sync::watch;

use crate::error::{CoreError, Result};
use crate::http::Http;

const SERVICE: &str = "the Joshflix relay";

/// Relay client.
#[derive(Clone)]
pub struct Relay {
    http: Http,
    base: String,
}

impl Relay {
    pub fn new(http: Http, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn create_pairing(&self) -> Result<CreatePairingResponse> {
        let response = self
            .http
            .client()
            .post(format!("{}{PAIRINGS_PATH}", self.base))
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        read(response).await
    }

    /// One long-poll. Returns the relay's current view of the pairing.
    pub async fn poll(&self, pairing: &CreatePairingResponse) -> Result<PairingPoll> {
        let response = self
            .http
            .client()
            .get(format!(
                "{}{PAIRINGS_PATH}/{}?wait={MAX_POLL_WAIT_SECS}",
                self.base, pairing.code
            ))
            .bearer_auth(&pairing.tv_secret)
            .timeout(Duration::from_secs(MAX_POLL_WAIT_SECS + 10))
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        read(response).await
    }

    /// Long-poll until the key arrives, the pairing expires, or `cancel` flips.
    /// Calls `on_update` with every poll result.
    pub async fn wait_for_key(
        &self,
        pairing: &CreatePairingResponse,
        mut cancel: watch::Receiver<bool>,
        on_update: impl Fn(&PairingPoll),
    ) -> Result<PairingPoll> {
        let mut failures = 0;
        loop {
            if *cancel.borrow() {
                let _ = self.cancel(pairing).await;
                return Err(CoreError::Cancelled);
            }
            let poll = tokio::select! {
                result = self.poll(pairing) => result,
                _ = cancel.wait_for(|c| *c) => {
                    let _ = self.cancel(pairing).await;
                    return Err(CoreError::Cancelled);
                }
            };
            match poll {
                Ok(poll) => {
                    failures = 0;
                    on_update(&poll);
                    use joshflix_protocol::PairingState::*;
                    match poll.state {
                        Delivered | Expired => return Ok(poll),
                        Pending | PhoneOpened => {}
                    }
                }
                Err(error) => {
                    failures += 1;
                    if failures >= 4 {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    pub async fn cancel(&self, pairing: &CreatePairingResponse) -> Result<()> {
        self.http
            .client()
            .delete(format!("{}{PAIRINGS_PATH}/{}", self.base, pairing.code))
            .bearer_auth(&pairing.tv_secret)
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        Ok(())
    }
}

pub(crate) async fn read<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| CoreError::Network { service: SERVICE, source })?;
    if !status.is_success() {
        let message = serde_json::from_slice::<ApiError>(&bytes)
            .map(|e| e.message)
            .unwrap_or_else(|_| format!("The Joshflix relay returned an error ({status})."));
        return Err(CoreError::Relay(message));
    }
    serde_json::from_slice(&bytes).map_err(|e| CoreError::decode(SERVICE, e))
}
