//! Wire types shared by the Joshflix TV app and the private relay server.
//!
//! The relay runs only on the dev box network. It does two jobs:
//!
//! 1. **Real-Debrid pairing.** The TV creates a pairing, shows a QR code that
//!    points to the relay's phone page, and long-polls until the phone submits
//!    an API key. The key is held in memory only, handed to the TV once, and
//!    then forgotten.
//! 2. **App updates.** The relay serves the latest signed APKs and a manifest
//!    the TV checks from Settings → About.
//!
//! Endpoints (all JSON unless noted):
//!
//! | Method | Path | Auth | Body / Response |
//! | --- | --- | --- | --- |
//! | `POST` | [`PAIRINGS_PATH`] | none | → [`CreatePairingResponse`] |
//! | `GET` | `/api/v1/pairings/{code}?wait=<secs>` | `Authorization: Bearer <tv_secret>` | → [`PairingPoll`] |
//! | `DELETE` | `/api/v1/pairings/{code}` | `Authorization: Bearer <tv_secret>` | → 204 |
//! | `GET` | `/p/{code}` | none | → HTML phone page |
//! | `GET` | `/api/v1/pairings/{code}/status` | none | → [`PhonePairingStatus`] |
//! | `POST` | `/api/v1/pairings/{code}/key` | none | [`SubmitKeyRequest`] → [`SubmitKeyResponse`] or [`ApiError`] |
//! | `GET` | [`UPDATE_MANIFEST_PATH`] | none | → [`UpdateManifest`] (404 when nothing is published) |
//! | `GET` | `/updates/{file}` | none | → APK bytes |
//! | `POST` | [`LOGS_PATH`] | none | plain-text log lines (header `x-joshflix-device`) → 204 |
//! | `GET` | `/healthz` | none | → `ok` |

use serde::{Deserialize, Serialize};

/// Path for creating a pairing.
pub const PAIRINGS_PATH: &str = "/api/v1/pairings";
/// Path of the update manifest.
pub const UPDATE_MANIFEST_PATH: &str = "/api/v1/updates/latest";
/// Path the TV posts its diagnostic log lines to.
pub const LOGS_PATH: &str = "/api/v1/logs";
/// Longest long-poll the relay honours, in seconds.
pub const MAX_POLL_WAIT_SECS: u64 = 30;
/// How long a pairing stays valid, in seconds.
pub const PAIRING_TTL_SECS: u64 = 600;
/// Length of the URL-safe pairing code.
pub const PAIRING_CODE_LEN: usize = 22;

/// Response to `POST /api/v1/pairings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePairingResponse {
    /// Random URL-safe code (base64url, [`PAIRING_CODE_LEN`] characters). Appears in the QR URL.
    pub code: String,
    /// Secret only the TV knows. Required to read the submitted key.
    pub tv_secret: String,
    /// Full URL the phone opens (the QR content), e.g. `http://192.168.1.67:8788/p/<code>`.
    pub phone_url: String,
    /// Seconds until the pairing expires.
    pub expires_in_secs: u64,
}

/// State of a pairing as seen by the TV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Waiting for the phone.
    Pending,
    /// The phone opened the page (the TV can say "Phone connected").
    PhoneOpened,
    /// The key is included in the first response with this state; the relay
    /// then forgets it (later polls carry only the username).
    Delivered,
    /// The pairing expired or was cancelled.
    Expired,
}

/// Response to the TV's long-poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingPoll {
    pub state: PairingState,
    /// Present only when `state` is [`PairingState::Delivered`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Real-Debrid username the relay verified, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Seconds until expiry.
    pub expires_in_secs: u64,
}

/// State of a pairing as seen by the phone page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhonePairingStatus {
    /// Ready to receive a key.
    Pending,
    /// A key was already sent to this TV.
    Submitted,
    /// Unknown or expired code.
    Missing,
}

/// Body of `POST /api/v1/pairings/{code}/key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitKeyRequest {
    pub api_key: String,
}

/// Successful response to a key submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitKeyResponse {
    /// Real-Debrid username for the key, shown on the phone as confirmation.
    pub username: String,
}

/// Error body for any non-2xx JSON response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Stable machine code, e.g. `invalid_key`, `expired`, `already_submitted`, `rate_limited`.
    pub code: String,
    /// Human-readable message safe to show to the user.
    pub message: String,
}

/// One APK in an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateApk {
    /// Android ABI: `armeabi-v7a`, `arm64-v8a` or `x86_64`.
    pub abi: String,
    /// Path relative to the relay base URL, e.g. `/updates/joshflix-0.2.0-arm64-v8a.apk`.
    pub path: String,
    /// Lowercase hex SHA-256 of the APK.
    pub sha256: String,
    /// Exact size in bytes.
    pub size: u64,
}

/// The latest published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub version_name: String,
    /// Android versionCode; strictly increasing.
    pub version_code: u64,
    /// Plain-text release notes.
    #[serde(default)]
    pub notes: String,
    /// RFC 3339 publish time.
    pub published_at: String,
    pub apks: Vec<UpdateApk>,
}

impl UpdateManifest {
    /// The APK matching the first ABI in `supported_abis` (Android's preference order).
    pub fn apk_for<'a>(&'a self, supported_abis: &[&str]) -> Option<&'a UpdateApk> {
        supported_abis
            .iter()
            .find_map(|abi| self.apks.iter().find(|apk| apk.abi == *abi))
    }
}

/// True when `code` has the shape of a pairing code (cheap check before any lookup).
pub fn is_valid_pairing_code(code: &str) -> bool {
    code.len() == PAIRING_CODE_LEN
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_supported_abi() {
        let manifest = UpdateManifest {
            version_name: "0.2.0".into(),
            version_code: 2,
            notes: String::new(),
            published_at: "2026-11-11T00:00:00Z".into(),
            apks: vec![
                UpdateApk { abi: "arm64-v8a".into(), path: "/a".into(), sha256: "x".into(), size: 1 },
                UpdateApk { abi: "armeabi-v7a".into(), path: "/b".into(), sha256: "y".into(), size: 2 },
            ],
        };
        assert_eq!(manifest.apk_for(&["armeabi-v7a", "armeabi"]).unwrap().path, "/b");
        assert_eq!(manifest.apk_for(&["arm64-v8a", "armeabi-v7a"]).unwrap().path, "/a");
        assert!(manifest.apk_for(&["x86_64"]).is_none());
    }

    #[test]
    fn validates_codes() {
        assert!(is_valid_pairing_code("abcdefghijklmnopqrstu_"));
        assert!(!is_valid_pairing_code("short"));
        assert!(!is_valid_pairing_code("abcdefghijklmnopqrst/_"));
    }

    #[test]
    fn poll_omits_absent_key() {
        let json = serde_json::to_string(&PairingPoll {
            state: PairingState::Pending,
            api_key: None,
            username: None,
            expires_in_secs: 5,
        })
        .unwrap();
        assert_eq!(json, r#"{"state":"pending","expires_in_secs":5}"#);
    }
}
