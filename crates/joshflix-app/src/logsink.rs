//! Ships the app's log lines to the relay on the dev box, so problems on a TV
//! without adb can still be diagnosed (`<relay data>/logs/tv.log`).
//!
//! Lines collect in a bounded in-memory buffer and go out in batches every few
//! seconds from a background task, so a stuck UI thread can't hold them back.
//! Secrets (the Real-Debrid key and the Comet config that embeds it) are
//! removed just before each batch leaves the TV.

// Only Android feeds the buffer (through `TeeLogger`).
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

/// Oldest lines are dropped beyond this (the relay may be unreachable).
const MAX_LINES: usize = 4000;
const FLUSH_EVERY: Duration = Duration::from_secs(3);

struct Buffer {
    lines: VecDeque<String>,
    dropped: usize,
    secrets: Vec<String>,
}

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer { lines: VecDeque::new(), dropped: 0, secrets: Vec::new() });

fn buffer() -> std::sync::MutexGuard<'static, Buffer> {
    BUFFER.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Queue one log line for upload.
pub fn record(level: &str, target: &str, message: &str) {
    let now = time_of_day();
    let line = format!("{now} {level:<5} {target}: {message}");
    let mut buf = buffer();
    if buf.lines.len() >= MAX_LINES {
        buf.lines.pop_front();
        buf.dropped += 1;
    }
    buf.lines.push_back(line);
}

/// Never upload `secret` (or anything containing it) in clear text.
pub fn add_secret(secret: &str) {
    let secret = secret.trim();
    // Short strings would redact ordinary words.
    if secret.len() < 12 {
        return;
    }
    let mut buf = buffer();
    if !buf.secrets.iter().any(|s| s == secret) {
        buf.secrets.push(secret.to_owned());
    }
}

/// Everything queued so far, redacted, or `None` when there's nothing new.
pub fn take_batch() -> Option<String> {
    let mut buf = buffer();
    if buf.lines.is_empty() {
        return None;
    }
    let mut text = String::new();
    if buf.dropped > 0 {
        text.push_str(&format!("({} older lines dropped)\n", buf.dropped));
        buf.dropped = 0;
    }
    for line in buf.lines.drain(..) {
        text.push_str(&line);
        text.push('\n');
    }
    for secret in &buf.secrets {
        text = text.replace(secret.as_str(), "<redacted>");
    }
    Some(text)
}

/// UTC wall-clock time as `HH:MM:SS.mmm` (the relay adds the date).
fn time_of_day() -> String {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", secs / 3600, secs / 60 % 60, secs % 60, since_epoch.subsec_millis())
}

/// Post batches to the relay every few seconds for the life of the runtime.
pub fn spawn_uploader(rt: &tokio::runtime::Handle, http: joshflix_core::http::Http, server: &str, device: &str) {
    let url = format!("{}{}", server.trim_end_matches('/'), joshflix_protocol::LOGS_PATH);
    // Header values must be visible ASCII.
    let device: String = format!("{device} - Joshflix {}", crate::app::VERSION_NAME)
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '-' })
        .collect();
    rt.spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Some(batch) = take_batch() else { continue };
            // Best effort: the relay only exists on the home network.
            let _ = http
                .client()
                .post(&url)
                .header("x-joshflix-device", &device)
                .header("content-type", "text/plain; charset=utf-8")
                .timeout(Duration::from_secs(10))
                .body(batch)
                .send()
                .await;
        }
    });
}

/// `log` backend for Android: logcat plus the upload buffer.
#[cfg(target_os = "android")]
pub struct TeeLogger {
    pub logcat: android_logger::AndroidLogger,
}

#[cfg(target_os = "android")]
impl log::Log for TeeLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.logcat.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        self.logcat.log(record);
        if record.level() <= log::Level::Info {
            crate::logsink::record(record.level().as_str(), record.target(), &record.args().to_string());
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_are_redacted_and_bounded() {
        add_secret("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345");
        add_secret("short");
        record("WARN", "joshflix", "GET https://comet/eyJABCDEFGHIJKLMNOPQRSTUVWXYZ012345/x failed");
        record("INFO", "joshflix", "a short word stays");
        let batch = take_batch().expect("batch");
        assert!(!batch.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"), "{batch}");
        assert!(batch.contains("<redacted>"));
        assert!(batch.contains("a short word stays"));
        assert!(take_batch().is_none());

        for i in 0..MAX_LINES + 5 {
            record("INFO", "t", &i.to_string());
        }
        let batch = take_batch().expect("batch");
        assert!(batch.starts_with("(5 older lines dropped)\n"), "{}", &batch[..60]);
        assert_eq!(batch.lines().count(), MAX_LINES + 1);
    }
}
