//! Private relay server for Joshflix.
//!
//! The relay runs on the dev box and is reachable only from the home LAN and
//! the tailnet. It never needs to be exposed to the internet. It has four jobs,
//! both described by the wire contract in [`joshflix_protocol`]:
//!
//! 1. **Real-Debrid key pairing.** Typing a 40-character API key with a TV
//!    remote is miserable, so the TV creates a pairing ([`pairing`]), shows a
//!    QR code that opens the relay's phone page ([`phone_page`]), and
//!    long-polls. The phone pastes its key, the relay checks it against
//!    Real-Debrid ([`realdebrid`]), and parks it in memory until the TV's next
//!    poll collects it. The key is handed over exactly once and then dropped;
//!    nothing touches disk.
//! 2. **App updates.** [`updates`] serves `updates/manifest.json` and the APKs
//!    next to it from the data directory. `cargo xtask publish` writes them.
//! 3. **Diagnostic logs.** TVs without adb post their log lines to [`logs`],
//!    which appends them to `logs/tv.log` in the data directory.
//! 4. **Screen colors.** While a video plays, the TV posts a tiny copy of the
//!    picture to [`screen`], which streams the color it reads as
//!    ([`palette`]) on a read-only WebSocket for the house lights.
//!
//! # Design
//!
//! * All pairing state lives in one `Mutex<HashMap>` inside [`AppState`]. The
//!   lock is never held across an `.await`, so a std mutex is fine and keeps
//!   the code simple.
//! * Each pairing owns a `tokio::sync::watch` sender that is bumped whenever
//!   its state changes. A long-poll subscribes under the lock, releases it,
//!   and waits for a bump, the poll deadline, or the pairing's expiry. The
//!   relay also remembers the last state it reported to the TV, so a poll
//!   returns at once when something happened between two polls.
//! * Codes are 16 random bytes and TV secrets 32 random bytes, both base64url.
//!   Only a SHA-256 hash of the TV secret is stored, and it is compared in
//!   constant time.
//! * Defense in depth: [`netguard`] rejects any peer outside loopback,
//!   private, Tailscale, and link-local ranges; [`ratelimit`] caps pairing
//!   creation and key submission per IP; the number of live pairings is
//!   capped.
//! * Logs never contain API keys, TV secrets, or full codes. A code appears
//!   only as its first four characters.
//!
//! [`router`] builds the whole app from a [`Config`], so tests can run it
//! without environment variables.

pub mod logs;
pub mod netguard;
pub mod pairing;
pub mod palette;
pub mod phone_page;
pub mod ratelimit;
pub mod realdebrid;
pub mod screen;
pub mod updates;

mod error;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{Router, middleware, routing::get};
use joshflix_protocol::{
    LOGS_PATH, MAX_POLL_WAIT_SECS, PAIRING_TTL_SECS, PAIRINGS_PATH, SCREEN_FEED_PATH, SCREEN_PATH,
    UPDATE_MANIFEST_PATH,
};

pub use error::api_error;

/// Default Real-Debrid REST base URL.
pub const DEFAULT_RD_API_BASE: &str = "https://api.real-debrid.com/rest/1.0";
/// Default listen address. IPv4 only on purpose: the dev box's IPv6 address is
/// globally routable. Port 8787 is taken by another service on the dev box.
pub const DEFAULT_BIND: &str = "0.0.0.0:8788";

/// Everything the relay needs to run. [`Config::from_env`] reads it from the
/// environment; tests build it directly.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root of the data directory. Updates live in `<data_dir>/updates/`.
    pub data_dir: PathBuf,
    /// Real-Debrid REST base URL, without a trailing slash.
    pub rd_api_base: String,
    /// Base URL the phone should open, e.g. `http://192.168.1.67:8788`.
    /// When `None`, the request's `Host` header is used.
    pub public_url: Option<String>,
    /// How long a pairing stays valid.
    pub pairing_ttl: Duration,
    /// Longest long-poll the relay honours.
    pub max_poll_wait: Duration,
    /// How long an expired pairing is kept so the TV sees `expired` instead of 404.
    pub expired_grace: Duration,
    /// Most pairings that may be live at once.
    pub max_active_pairings: usize,
    /// Pairing creations allowed per IP per window.
    pub create_limit: ratelimit::Limit,
    /// Key submissions allowed per IP per window.
    pub submit_limit: ratelimit::Limit,
    /// Timeout for calls to Real-Debrid.
    pub rd_timeout: Duration,
}

impl Config {
    /// Defaults suitable for production, with the given data directory.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            rd_api_base: DEFAULT_RD_API_BASE.to_owned(),
            public_url: None,
            pairing_ttl: Duration::from_secs(PAIRING_TTL_SECS),
            max_poll_wait: Duration::from_secs(MAX_POLL_WAIT_SECS),
            expired_grace: Duration::from_secs(60),
            max_active_pairings: 256,
            create_limit: ratelimit::Limit {
                max: 30,
                per: Duration::from_secs(60),
            },
            submit_limit: ratelimit::Limit {
                max: 10,
                per: Duration::from_secs(60),
            },
            rd_timeout: Duration::from_secs(10),
        }
    }

    /// Reads `JOSHFLIX_RELAY_DATA`, `JOSHFLIX_RD_API_BASE`, and
    /// `JOSHFLIX_RELAY_PUBLIC_URL`. Missing values fall back to defaults.
    pub fn from_env() -> Result<Self, String> {
        let data_dir = match non_empty_env("JOSHFLIX_RELAY_DATA") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = non_empty_env("HOME")
                    .ok_or("HOME is not set; set JOSHFLIX_RELAY_DATA instead")?;
                PathBuf::from(home).join(".local/share/joshflix-relay")
            }
        };
        let mut config = Self::new(data_dir);
        if let Some(base) = non_empty_env("JOSHFLIX_RD_API_BASE") {
            config.rd_api_base = base.trim_end_matches('/').to_owned();
        }
        if let Some(url) = non_empty_env("JOSHFLIX_RELAY_PUBLIC_URL") {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("JOSHFLIX_RELAY_PUBLIC_URL must start with http:// or https://".into());
            }
            config.public_url = Some(url.trim_end_matches('/').to_owned());
        }
        Ok(config)
    }

    /// Directory holding the update manifest and APKs.
    pub fn updates_dir(&self) -> PathBuf {
        self.data_dir.join("updates")
    }

    /// Directory holding the TVs' diagnostic logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Shared state behind every handler. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    pairings: Mutex<HashMap<String, pairing::Pairing>>,
    rd: realdebrid::RealDebrid,
    create_limiter: ratelimit::RateLimiter,
    submit_limiter: ratelimit::RateLimiter,
    /// Serializes appends to the TV log file.
    logs_lock: tokio::sync::Mutex<()>,
    /// What plays on the TV and its colors.
    screen: screen::Screen,
}

impl AppState {
    /// Builds the state, including the HTTPS client used to reach Real-Debrid.
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        let rd = realdebrid::RealDebrid::new(&config.rd_api_base, config.rd_timeout)?;
        Ok(Self {
            inner: Arc::new(Inner {
                create_limiter: ratelimit::RateLimiter::new(config.create_limit),
                submit_limiter: ratelimit::RateLimiter::new(config.submit_limit),
                pairings: Mutex::new(HashMap::new()),
                logs_lock: tokio::sync::Mutex::new(()),
                screen: screen::Screen::default(),
                rd,
                config,
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    fn pairings(&self) -> MutexGuard<'_, HashMap<String, pairing::Pairing>> {
        // A panic while holding the lock cannot leave a pairing half-written in a
        // way that matters more than refusing service, so recover from poisoning.
        self.inner
            .pairings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Drops pairings past their expiry grace and stale rate-limit windows.
    pub fn sweep(&self) {
        let removed = pairing::sweep(self);
        self.inner.create_limiter.sweep();
        self.inner.submit_limiter.sweep();
        if removed > 0 {
            tracing::debug!(removed, "swept expired pairings");
        }
    }

    /// Marks playback stopped every few seconds once the TV has gone quiet,
    /// until the runtime shuts down.
    pub fn spawn_screen_watchdog(&self) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                state.inner.screen.expire(std::time::Instant::now());
            }
        })
    }

    /// Runs [`AppState::sweep`] every `every` until the runtime shuts down.
    pub fn spawn_sweeper(&self, every: Duration) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                state.sweep();
            }
        })
    }
}

/// The whole relay as an axum router. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`: the network guard
/// rejects requests that carry no peer address.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(PAIRINGS_PATH, axum::routing::post(pairing::create))
        .route(
            "/api/v1/pairings/{code}",
            get(pairing::poll).delete(pairing::cancel),
        )
        .route("/api/v1/pairings/{code}/status", get(pairing::phone_status))
        .route(
            "/api/v1/pairings/{code}/key",
            axum::routing::post(pairing::submit_key)
                .layer(axum::extract::DefaultBodyLimit::max(4 * 1024)),
        )
        .route("/p/{code}", get(phone_page::page))
        .route(UPDATE_MANIFEST_PATH, get(updates::manifest))
        .route("/updates/{file}", get(updates::apk))
        .route(
            LOGS_PATH,
            axum::routing::post(logs::upload)
                .layer(axum::extract::DefaultBodyLimit::max(logs::MAX_BATCH_BYTES)),
        )
        .route(
            SCREEN_PATH,
            axum::routing::post(screen::report)
                .layer(axum::extract::DefaultBodyLimit::max(screen::MAX_REPORT_BYTES)),
        )
        .route(SCREEN_FEED_PATH, get(screen::feed))
        .fallback(|| async {
            api_error(
                axum::http::StatusCode::NOT_FOUND,
                "not_found",
                "No such endpoint.",
            )
        })
        .layer(middleware::from_fn(netguard::guard))
        .with_state(state)
}
