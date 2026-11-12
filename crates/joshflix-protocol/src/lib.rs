//! Wire types shared by the Joshflix TV app and the private relay server.
//!
//! The relay runs only on the dev box network. Its jobs:
//!
//! 1. **Real-Debrid pairing.** The TV creates a pairing, shows a QR code that
//!    points to the relay's phone page, and long-polls until the phone submits
//!    an API key. The key is held in memory only, handed to the TV once, and
//!    then forgotten.
//! 2. **App updates.** The relay serves the latest signed APKs and a manifest
//!    the TV checks from Settings → About.
//! 3. **TV logs.** TVs without adb post their log lines.
//! 4. **Screen colors.** While a video plays, the TV posts a tiny copy of the
//!    picture; the relay streams the color it reads as to the house lights.
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
//! | `POST` | [`SCREEN_PATH`] | none | [`ScreenReport`] → [`ScreenReportResponse`] |
//! | `GET` | [`SCREEN_FEED_PATH`] | none | WebSocket of [`ScreenFeedFrame`] JSON text frames |
//! | `GET` | `/healthz` | none | → `ok` |

use serde::{Deserialize, Serialize};

/// Path for creating a pairing.
pub const PAIRINGS_PATH: &str = "/api/v1/pairings";
/// Path of the update manifest.
pub const UPDATE_MANIFEST_PATH: &str = "/api/v1/updates/latest";
/// Path the TV posts its diagnostic log lines to.
pub const LOGS_PATH: &str = "/api/v1/logs";
/// Path the TV posts what plays and a tiny copy of the picture to.
pub const SCREEN_PATH: &str = "/api/v1/screen";
/// Read-only WebSocket with the screen's colors, for the house lights.
pub const SCREEN_FEED_PATH: &str = "/api/v1/screen/feed";
/// Size of the picture sample the TV sends (16:9).
pub const SCREEN_SAMPLE_WIDTH: u32 = 48;
pub const SCREEN_SAMPLE_HEIGHT: u32 = 27;
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

/// `POST /api/v1/screen`: what the TV is playing, and a tiny copy of the picture.
///
/// The TV posts whenever playback starts, pauses or stops, and while it plays:
/// about once a second with a [`ScreenSample`] while the relay says someone
/// wants the colors, otherwise every few seconds without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenReport {
    /// Video is playing (not paused, buffering or stopped).
    pub playing: bool,
    /// What plays, e.g. `"The Office · S2E4"`. `None` when nothing does.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub sample: Option<ScreenSample>,
}

/// The picture scaled down to `width` × `height`, as packed 8-bit RGB rows,
/// base64 encoded (standard alphabet, padded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSample {
    pub width: u32,
    pub height: u32,
    pub rgb: String,
}

/// Response to a [`ScreenReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenReportResponse {
    /// Someone listens to the colors: send a [`ScreenSample`] about once a second.
    pub wanted: bool,
}

/// One frame of the screen colors feed ([`SCREEN_FEED_PATH`]). Sent when a
/// listener connects and after every change. Shaped like the music server's
/// `/now-playing` frame, which the lights also follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum ScreenFeedFrame {
    ScreenColors {
        /// Video is playing on the TV right now.
        playing: bool,
        /// What plays, or `None`.
        title: Option<String>,
        /// The one color the picture reads as, lowercase `#rrggbb`. `None`
        /// when it has no usable color (dark or grey): the lights should hold
        /// theirs.
        color: Option<String>,
    },
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
    fn screen_frames_carry_a_tag() {
        let frame = ScreenFeedFrame::ScreenColors {
            playing: true,
            title: Some("Dune".into()),
            color: Some("#c8742f".into()),
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r##"{"_tag":"ScreenColors","playing":true,"title":"Dune","color":"#c8742f"}"##
        );
        let dark = ScreenFeedFrame::ScreenColors { playing: false, title: None, color: None };
        assert_eq!(
            serde_json::to_string(&dark).unwrap(),
            r#"{"_tag":"ScreenColors","playing":false,"title":null,"color":null}"#
        );
        let report: ScreenReport = serde_json::from_str(r#"{"playing":false}"#).unwrap();
        assert_eq!(report, ScreenReport { playing: false, title: None, sample: None });
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
