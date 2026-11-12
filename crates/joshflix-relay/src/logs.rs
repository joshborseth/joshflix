//! Diagnostic logs from TVs.
//!
//! TVs often have no adb, so the app batches its own log lines (with secrets
//! already redacted) and posts them here. They're appended to
//! `<data_dir>/logs/tv.log`, which rotates to `tv.log.1` past
//! [`MAX_LOG_BYTES`]. Only LAN and tailnet peers get this far (see
//! [`crate::netguard`]).

use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::io::AsyncWriteExt as _;

use crate::{AppState, api_error, netguard::canonical_ip};

/// Largest accepted batch.
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
/// `tv.log` rotates once it grows past this.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Header naming the device ("Skyworth … · Android 12 · 0.1.3").
pub const DEVICE_HEADER: &str = "x-joshflix-device";

/// `POST /api/v1/logs`: a plain-text batch of log lines.
pub async fn upload(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let device: String = headers
        .get(DEVICE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown device")
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let text = String::from_utf8_lossy(&body);
    let ip = canonical_ip(peer.ip());
    let mut batch = String::with_capacity(text.len() + 128);
    batch.push_str(&format!("==== {ip} · {device}\n"));
    for line in text.lines() {
        batch.push_str(line.trim_end_matches('\r'));
        batch.push('\n');
    }
    match append(&state, batch.as_bytes()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "cannot write TV logs");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "log_write_failed", "The logs could not be saved.")
        }
    }
}

async fn append(state: &AppState, bytes: &[u8]) -> std::io::Result<()> {
    let dir = state.config().logs_dir();
    let path = dir.join("tv.log");
    // Serialize writers so batches never interleave and rotation is atomic.
    let _guard = state.inner.logs_lock.lock().await;
    tokio::fs::create_dir_all(&dir).await?;
    if tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        tokio::fs::rename(&path, dir.join("tv.log.1")).await?;
    }
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
    file.write_all(bytes).await?;
    file.flush().await
}
