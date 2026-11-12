//! Real-Debrid key pairing between the TV and a phone.
//!
//! Lifecycle of one pairing:
//!
//! ```text
//! Pending ──phone opens page──▶ PhoneOpened ──phone submits valid key──▶ KeyWaiting
//!    │                              │                                       │
//!    │                              │                          TV poll takes the key
//!    │                              │                                       ▼
//!    └──────── TTL or DELETE ───────┴──────────────▶ Expired         Delivered (key forgotten)
//! ```
//!
//! The TV sees `pending`, `phone_opened`, `delivered`, and `expired`. The phone
//! sees `pending` until a key is accepted and `submitted` after that.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use joshflix_protocol::{
    CreatePairingResponse, PairingPoll, PairingState, PhonePairingStatus, SubmitKeyRequest,
    SubmitKeyResponse, is_valid_pairing_code,
};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::{
    AppState, api_error,
    error::json_no_store,
    realdebrid::{RdError, key_looks_valid},
};

/// Message shown on the phone for unknown or expired codes.
const EXPIRED_MESSAGE: &str =
    "This code is invalid or expired. Go back to the TV and scan a new QR code.";

pub(crate) struct Pairing {
    secret_hash: [u8; 32],
    expires_at: Instant,
    phase: Phase,
    /// A Real-Debrid check for this pairing is in flight.
    checking: bool,
    /// Last state reported to the TV, so a poll returns at once after a change.
    tv_seen: PairingState,
    /// Bumped on every state change; long-polls wait on it. Dropping the
    /// pairing drops the sender, which also wakes them.
    changed: watch::Sender<()>,
}

enum Phase {
    Pending,
    PhoneOpened,
    KeyWaiting { api_key: String, username: String },
    Delivered { username: String },
}

impl Pairing {
    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    fn expires_in_secs(&self, now: Instant) -> u64 {
        // Round up so a live pairing never reports 0.
        let left = self.expires_at.saturating_duration_since(now);
        left.as_secs() + u64::from(left.subsec_nanos() > 0)
    }

    fn secret_matches(&self, presented: &str) -> bool {
        constant_time_eq(&self.secret_hash, &hash_secret(presented))
    }

    fn key_submitted(&self) -> bool {
        matches!(
            self.phase,
            Phase::KeyWaiting { .. } | Phase::Delivered { .. }
        )
    }

    fn notify(&self) {
        self.changed.send_replace(());
    }
}

fn hash_secret(secret: &str) -> [u8; 32] {
    let digest = Sha256::digest(secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn random_token<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// First four characters of a code, safe to log.
fn tag(code: &str) -> &str {
    code.get(..4).unwrap_or("????")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

fn expired_response() -> Response {
    api_error(StatusCode::NOT_FOUND, "expired", EXPIRED_MESSAGE)
}

fn unauthorized() -> Response {
    api_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Missing or wrong TV secret.",
    )
}

fn rate_limited() -> Response {
    api_error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many attempts. Wait a minute and try again.",
    )
}

/// Removes pairings whose expiry grace has passed. Returns how many went.
pub(crate) fn sweep(state: &AppState) -> usize {
    let grace = state.config().expired_grace;
    let now = Instant::now();
    let mut pairings = state.pairings();
    let before = pairings.len();
    pairings.retain(|_, p| now < p.expires_at + grace);
    before - pairings.len()
}

/// `true` when `host` is a plausible `host[:port]` from a `Host` header.
pub fn is_sane_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        let ok = !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
        if !ok {
            return false;
        }
        match after {
            "" => return true,
            _ => match after.strip_prefix(':') {
                Some(port) => (None, Some(port)),
                None => return false,
            },
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) => (Some(name), Some(port)),
            None => (Some(host), None),
        }
    };
    if let Some(name) = name {
        let ok = !name.is_empty()
            && !name.starts_with(['-', '.'])
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !ok {
            return false;
        }
    }
    match port {
        None => true,
        Some(port) => {
            (1..=5).contains(&port.len())
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|p| p != 0)
        }
    }
}

/// `POST /api/v1/pairings`
pub async fn create(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let config = state.config();
    let base = match &config.public_url {
        Some(url) => url.clone(),
        None => {
            let host = headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            if !is_sane_host(host) {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "bad_host",
                    "The request's Host header is missing or invalid.",
                );
            }
            format!("http://{host}")
        }
    };
    if !state.inner.create_limiter.check(peer.ip()) {
        tracing::warn!(ip = %peer.ip(), "pairing create rate limited");
        return rate_limited();
    }

    let code = random_token::<16>();
    let tv_secret = random_token::<32>();
    let ttl = config.pairing_ttl;
    let now = Instant::now();
    {
        let grace = config.expired_grace;
        let mut pairings = state.pairings();
        pairings.retain(|_, p| now < p.expires_at + grace);
        let active = pairings.values().filter(|p| !p.is_expired(now)).count();
        if active >= config.max_active_pairings {
            tracing::warn!(active, "too many active pairings");
            return rate_limited();
        }
        let (changed, _) = watch::channel(());
        pairings.insert(
            code.clone(),
            Pairing {
                secret_hash: hash_secret(&tv_secret),
                expires_at: now + ttl,
                phase: Phase::Pending,
                checking: false,
                tv_seen: PairingState::Pending,
                changed,
            },
        );
    }
    tracing::info!(code = tag(&code), ip = %peer.ip(), "pairing created");
    let phone_url = format!("{base}/p/{code}");
    json_no_store(
        StatusCode::CREATED,
        CreatePairingResponse {
            code,
            tv_secret,
            phone_url,
            expires_in_secs: ttl.as_secs(),
        },
    )
}

fn parse_wait(query: Option<&str>) -> u64 {
    query
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("wait="))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

fn poll_body(state: PairingState, expires_in_secs: u64) -> Response {
    json_no_store(
        StatusCode::OK,
        PairingPoll {
            state,
            api_key: None,
            username: None,
            expires_in_secs,
        },
    )
}

/// `GET /api/v1/pairings/{code}?wait=<secs>` (TV, Bearer `tv_secret`)
pub async fn poll(
    State(state): State<AppState>,
    Path(code): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !is_valid_pairing_code(&code) {
        return expired_response();
    }
    let Some(secret) = bearer(&headers) else {
        return unauthorized();
    };
    let wait = Duration::from_secs(parse_wait(query.as_deref())).min(state.config().max_poll_wait);
    let deadline = Instant::now() + wait;
    let mut first = true;

    loop {
        let (mut changed, expires_at) = {
            let mut pairings = state.pairings();
            let Some(pairing) = pairings.get_mut(&code) else {
                // Gone while we waited means cancelled or swept: report expiry.
                return if first {
                    expired_response()
                } else {
                    poll_body(PairingState::Expired, 0)
                };
            };
            if first && !pairing.secret_matches(secret) {
                tracing::warn!(code = tag(&code), "poll with wrong TV secret");
                return unauthorized();
            }
            first = false;
            let now = Instant::now();
            if pairing.is_expired(now) {
                pairing.tv_seen = PairingState::Expired;
                return poll_body(PairingState::Expired, 0);
            }
            let expires_in_secs = pairing.expires_in_secs(now);
            let visible = match &pairing.phase {
                Phase::Pending => PairingState::Pending,
                Phase::PhoneOpened => PairingState::PhoneOpened,
                Phase::Delivered { username } => {
                    // Already handed over; repeat the outcome without the key.
                    return json_no_store(
                        StatusCode::OK,
                        PairingPoll {
                            state: PairingState::Delivered,
                            api_key: None,
                            username: Some(username.clone()),
                            expires_in_secs,
                        },
                    );
                }
                Phase::KeyWaiting { username, .. } => {
                    let delivered = Phase::Delivered {
                        username: username.clone(),
                    };
                    let Phase::KeyWaiting { api_key, username } =
                        std::mem::replace(&mut pairing.phase, delivered)
                    else {
                        // Not reachable: the arm above matched KeyWaiting under the same lock.
                        return poll_body(PairingState::Expired, 0);
                    };
                    pairing.tv_seen = PairingState::Delivered;
                    pairing.notify();
                    tracing::info!(code = tag(&code), "key delivered to TV");
                    return json_no_store(
                        StatusCode::OK,
                        PairingPoll {
                            state: PairingState::Delivered,
                            api_key: Some(api_key),
                            username: Some(username),
                            expires_in_secs,
                        },
                    );
                }
            };
            if visible != pairing.tv_seen || now >= deadline {
                pairing.tv_seen = visible;
                return poll_body(visible, expires_in_secs);
            }
            (pairing.changed.subscribe(), pairing.expires_at)
        };
        let until = tokio::time::Instant::from_std(deadline.min(expires_at));
        // Either outcome means "look again": a change, the pairing vanished
        // (sender dropped), or the deadline/expiry passed.
        let _ = tokio::time::timeout_at(until, changed.changed()).await;
    }
}

/// `DELETE /api/v1/pairings/{code}` (TV, Bearer `tv_secret`)
pub async fn cancel(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_pairing_code(&code) {
        return expired_response();
    }
    let Some(secret) = bearer(&headers) else {
        return unauthorized();
    };
    let mut pairings = state.pairings();
    match pairings.get(&code) {
        None => expired_response(),
        Some(p) if !p.secret_matches(secret) => unauthorized(),
        Some(_) => {
            pairings.remove(&code);
            tracing::info!(code = tag(&code), "pairing cancelled");
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Phone-side view of a pairing. Marks it as opened, which wakes the TV.
pub(crate) fn phone_touch(state: &AppState, code: &str) -> PhonePairingStatus {
    if !is_valid_pairing_code(code) {
        return PhonePairingStatus::Missing;
    }
    let mut pairings = state.pairings();
    let Some(pairing) = pairings.get_mut(code) else {
        return PhonePairingStatus::Missing;
    };
    if pairing.is_expired(Instant::now()) {
        return PhonePairingStatus::Missing;
    }
    match pairing.phase {
        Phase::Pending => {
            pairing.phase = Phase::PhoneOpened;
            pairing.notify();
            tracing::info!(code = tag(code), "phone opened pairing");
            PhonePairingStatus::Pending
        }
        Phase::PhoneOpened => PhonePairingStatus::Pending,
        Phase::KeyWaiting { .. } | Phase::Delivered { .. } => PhonePairingStatus::Submitted,
    }
}

/// `GET /api/v1/pairings/{code}/status` (phone)
pub async fn phone_status(State(state): State<AppState>, Path(code): Path<String>) -> Response {
    json_no_store(StatusCode::OK, phone_touch(&state, &code))
}

/// Clears the `checking` flag even if the request future is dropped mid-check.
struct CheckingGuard<'a> {
    state: &'a AppState,
    code: &'a str,
}

impl Drop for CheckingGuard<'_> {
    fn drop(&mut self) {
        if let Some(pairing) = self.state.pairings().get_mut(self.code) {
            pairing.checking = false;
        }
    }
}

/// `POST /api/v1/pairings/{code}/key` (phone)
pub async fn submit_key(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(code): Path<String>,
    body: Bytes,
) -> Response {
    if !state.inner.submit_limiter.check(peer.ip()) {
        tracing::warn!(ip = %peer.ip(), "key submit rate limited");
        return rate_limited();
    }
    if !is_valid_pairing_code(&code) {
        return expired_response();
    }
    let Ok(request) = serde_json::from_slice::<SubmitKeyRequest>(&body) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Expected {\"api_key\": \"…\"}.",
        );
    };
    let api_key = request.api_key.trim();

    {
        let mut pairings = state.pairings();
        let Some(pairing) = pairings.get_mut(&code) else {
            return expired_response();
        };
        if pairing.is_expired(Instant::now()) {
            return expired_response();
        }
        if pairing.key_submitted() {
            return already_submitted();
        }
        if pairing.checking {
            return api_error(
                StatusCode::CONFLICT,
                "check_in_progress",
                "A key for this TV is already being checked. Wait a moment.",
            );
        }
        if !key_looks_valid(api_key) {
            return invalid_key();
        }
        pairing.checking = true;
    }
    let guard = CheckingGuard {
        state: &state,
        code: &code,
    };
    let result = state.inner.rd.verify(api_key).await;
    drop(guard);

    let username = match result {
        Ok(username) => username,
        Err(RdError::InvalidKey) => {
            tracing::info!(code = tag(&code), "Real-Debrid rejected key");
            return invalid_key();
        }
        Err(RdError::Unreachable) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "rd_unreachable",
                "Couldn't reach Real-Debrid to check the key. Try again in a moment.",
            );
        }
    };

    let mut pairings = state.pairings();
    let Some(pairing) = pairings.get_mut(&code) else {
        return expired_response();
    };
    if pairing.is_expired(Instant::now()) {
        return expired_response();
    }
    if pairing.key_submitted() {
        return already_submitted();
    }
    pairing.phase = Phase::KeyWaiting {
        api_key: api_key.to_owned(),
        username: username.clone(),
    };
    pairing.notify();
    tracing::info!(code = tag(&code), "key accepted, waiting for TV");
    json_no_store(StatusCode::OK, SubmitKeyResponse { username })
}

fn already_submitted() -> Response {
    api_error(
        StatusCode::CONFLICT,
        "already_submitted",
        "This TV already received a key. You can go back to the TV.",
    )
}

fn invalid_key() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_key",
        "Real-Debrid didn't accept that key. Copy it again from real-debrid.com/apitoken.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_expected_shape() {
        for _ in 0..100 {
            let code = random_token::<16>();
            assert!(is_valid_pairing_code(&code), "{code}");
            assert_eq!(random_token::<32>().len(), 43);
        }
        assert_ne!(random_token::<16>(), random_token::<16>());
    }

    #[test]
    fn secret_comparison() {
        let hash = hash_secret("abc");
        assert!(constant_time_eq(&hash, &hash_secret("abc")));
        assert!(!constant_time_eq(&hash, &hash_secret("abd")));
        assert!(!constant_time_eq(&hash, &hash_secret("")));
    }

    #[test]
    fn parses_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer s3cret".parse().expect("header"),
        );
        assert_eq!(bearer(&headers), Some("s3cret"));
        headers.insert(
            header::AUTHORIZATION,
            "bearer  s3cret ".parse().expect("header"),
        );
        assert_eq!(bearer(&headers), Some("s3cret"));
        headers.insert(
            header::AUTHORIZATION,
            "Basic s3cret".parse().expect("header"),
        );
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer ".parse().expect("header"));
        assert_eq!(bearer(&headers), None);
    }

    #[test]
    fn parses_wait() {
        assert_eq!(parse_wait(None), 0);
        assert_eq!(parse_wait(Some("wait=25")), 25);
        assert_eq!(parse_wait(Some("x=1&wait=7")), 7);
        assert_eq!(parse_wait(Some("wait=-1")), 0);
        assert_eq!(parse_wait(Some("wait=abc")), 0);
    }

    #[test]
    fn host_validation() {
        for ok in [
            "192.168.1.67:8788",
            "localhost",
            "dev-box.local:80",
            "[::1]:8788",
            "[fd7a::1]",
            "100.107.121.43",
        ] {
            assert!(is_sane_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.com/path",
            "a b",
            "host:",
            "host:0",
            "host:99999",
            "host:12a",
            "-host",
            ".host",
            "[::1",
            "[::1]x",
            "[zz]",
            "user@host",
            "host\"><script>",
        ] {
            assert!(!is_sane_host(bad), "{bad}");
        }
    }

    #[test]
    fn code_tag_is_short() {
        assert_eq!(tag("abcdefghijklmnopqrstuv"), "abcd");
        assert_eq!(tag("ab"), "????");
    }
}
