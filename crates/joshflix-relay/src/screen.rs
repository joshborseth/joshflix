//! Screen colors for the house lights.
//!
//! While a video plays, the TV posts what plays and a tiny copy of the picture
//! ([`report`]). The relay finds the one color the picture reads as
//! ([`crate::palette`]) and streams it on a read-only WebSocket ([`feed`]) that
//! the lights app listens to. The TV only sends pictures while someone listens: each report's answer
//! says whether anyone does.
//!
//! The color is smoothed here, so the lights don't flicker with every
//! explosion or camera cut: a new color goes out only once the picture's color
//! has clearly changed ([`CHANGE`]) and stayed that way for [`HOLD`], and at
//! most once every [`MIN_GAP`]. Play state changes go out at once.

use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use joshflix_protocol::{ScreenFeedFrame, ScreenReport, ScreenReportResponse, ScreenSample};
use tokio::sync::watch;

use crate::{
    AppState, api_error,
    palette::{self, Lab},
};

/// Colors closer than this (OKLab distance) look the same on a bulb.
pub const CHANGE: f32 = 0.07;
/// A new color must hold this long before it goes out.
pub const HOLD: Duration = Duration::from_millis(1500);
/// Color changes go out at most this often.
pub const MIN_GAP: Duration = Duration::from_secs(2);
/// A TV that hasn't reported for this long has stopped playing (closed, asleep, gone).
pub const SILENCE: Duration = Duration::from_secs(20);
/// Largest picture sample accepted.
const MAX_SAMPLE_PIXELS: u32 = 64 * 64;
/// Largest accepted report.
pub const MAX_REPORT_BYTES: usize = 32 * 1024;

/// What the relay knows about the screen, and the feed listeners.
pub struct Screen {
    tracker: Mutex<Tracker>,
    feed: watch::Sender<ScreenFeedFrame>,
    listeners: AtomicUsize,
}

impl Default for Screen {
    fn default() -> Self {
        let tracker = Tracker::default();
        let (feed, _) = watch::channel(tracker.frame());
        Self {
            tracker: Mutex::new(tracker),
            feed,
            listeners: AtomicUsize::new(0),
        }
    }
}

impl Screen {
    fn tracker(&self) -> MutexGuard<'_, Tracker> {
        self.tracker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Someone listens to the feed.
    pub fn wanted(&self) -> bool {
        self.listeners.load(Ordering::Relaxed) > 0
    }

    /// Handle a TV report taken at `now`. `color` is `None` without a
    /// picture, `Some(None)` for a picture without a usable color.
    pub fn report(&self, playing: bool, title: Option<String>, color: Option<Option<Lab>>, now: Instant) {
        let frame = {
            let mut tracker = self.tracker();
            if !tracker.report(playing, title, color, now) {
                return;
            }
            tracker.frame()
        };
        self.publish(frame);
    }

    /// Mark playback stopped when the TV has gone quiet.
    pub fn expire(&self, now: Instant) {
        let frame = {
            let mut tracker = self.tracker();
            if !tracker.expire(now) {
                return;
            }
            tracing::info!("screen: the TV stopped reporting; treating playback as stopped");
            tracker.frame()
        };
        self.publish(frame);
    }

    fn publish(&self, frame: ScreenFeedFrame) {
        self.feed.send_if_modified(|current| {
            if *current == frame {
                return false;
            }
            *current = frame;
            true
        });
    }

    /// The current frame, then every change.
    pub fn subscribe(&self) -> watch::Receiver<ScreenFeedFrame> {
        self.feed.subscribe()
    }

    /// Count a feed listener until the guard drops. The first listener
    /// forgets the color from the last time someone listened (the TV sent no
    /// pictures since), so the next picture goes out at once.
    fn listen(&self) -> ListenerGuard<'_> {
        if self.listeners.fetch_add(1, Ordering::Relaxed) == 0 {
            let frame = {
                let mut tracker = self.tracker();
                tracker.forget_color();
                tracker.frame()
            };
            self.publish(frame);
        }
        ListenerGuard(self)
    }
}

struct ListenerGuard<'a>(&'a Screen);

impl Drop for ListenerGuard<'_> {
    fn drop(&mut self) {
        self.0.listeners.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Play state and color smoothing. Pure: time comes in as `now`.
#[derive(Debug, Default)]
pub struct Tracker {
    playing: bool,
    title: Option<String>,
    last_report: Option<Instant>,
    /// The color that went out and when; `None` until the first picture
    /// since playback (re)started, which goes out at once.
    published: Option<(Option<Lab>, Instant)>,
    /// A different color waiting to hold for [`HOLD`], and since when.
    pending: Option<(Option<Lab>, Instant)>,
}

impl Tracker {
    /// Returns whether the feed's frame changed.
    pub fn report(
        &mut self,
        playing: bool,
        title: Option<String>,
        color: Option<Option<Lab>>,
        now: Instant,
    ) -> bool {
        self.last_report = Some(now);
        let mut changed = false;
        if playing != self.playing || title != self.title {
            if playing && !self.playing {
                self.published = None;
                self.pending = None;
            }
            if playing != self.playing {
                let what = title.as_deref().unwrap_or("something");
                tracing::info!("screen: {} {what}", if playing { "playing" } else { "not playing" });
            }
            self.playing = playing;
            self.title = title;
            changed = true;
        }
        if playing && let Some(color) = color {
            changed |= self.consider(color, now);
        }
        changed
    }

    fn consider(&mut self, color: Option<Lab>, now: Instant) -> bool {
        let Some((current, published_at)) = self.published else {
            self.published = Some((color, now));
            self.pending = None;
            return true;
        };
        if difference(color, current) < CHANGE {
            self.pending = None;
            return false;
        }
        let since = match self.pending {
            Some((waiting, since)) if difference(color, waiting) < CHANGE => since,
            _ => now,
        };
        if now.duration_since(since) >= HOLD && now.duration_since(published_at) >= MIN_GAP {
            tracing::debug!(color = color.map(palette::hex).unwrap_or_default(), "screen: new color");
            self.published = Some((color, now));
            self.pending = None;
            return true;
        }
        self.pending = Some((color, since));
        false
    }

    /// Drop the published color: the next picture goes out at once.
    pub fn forget_color(&mut self) {
        self.published = None;
        self.pending = None;
    }

    /// Returns whether the feed's frame changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let quiet = self
            .last_report
            .is_none_or(|at| now.duration_since(at) >= SILENCE);
        if !quiet || (!self.playing && self.title.is_none()) {
            return false;
        }
        self.playing = false;
        self.title = None;
        true
    }

    pub fn frame(&self) -> ScreenFeedFrame {
        ScreenFeedFrame::ScreenColors {
            playing: self.playing,
            title: self.title.clone(),
            color: self
                .published
                .and_then(|(color, _)| color)
                .map(palette::hex),
        }
    }
}

/// How far apart two colors look; no color is infinitely far from any color.
fn difference(x: Option<Lab>, y: Option<Lab>) -> f32 {
    match (x, y) {
        (None, None) => 0.0,
        (Some(x), Some(y)) => x.distance(y),
        _ => f32::INFINITY,
    }
}

/// The sample's pixels, or why they can't be used.
fn decode(sample: &ScreenSample) -> Result<Vec<u8>, &'static str> {
    let pixels = sample.width.checked_mul(sample.height).ok_or("too big")?;
    if pixels == 0 || pixels > MAX_SAMPLE_PIXELS {
        return Err("picture sample has the wrong size");
    }
    let rgb = base64::engine::general_purpose::STANDARD
        .decode(&sample.rgb)
        .map_err(|_| "picture sample is not base64")?;
    if rgb.len() != pixels as usize * 3 {
        return Err("picture sample length doesn't match its size");
    }
    Ok(rgb)
}

/// `POST /api/v1/screen`.
pub async fn report(State(state): State<AppState>, Json(report): Json<ScreenReport>) -> Response {
    let color = match report.sample.as_ref().map(decode).transpose() {
        Ok(rgb) => rgb.map(|rgb| palette::extract(&rgb).first().map(|s| s.lab)),
        Err(message) => return api_error(StatusCode::BAD_REQUEST, "bad_sample", message),
    };
    let title = report
        .title
        .map(|t| t.chars().filter(|c| !c.is_control()).take(200).collect::<String>())
        .filter(|t| !t.trim().is_empty());
    let screen = &state.inner.screen;
    screen.report(report.playing, title, color, Instant::now());
    Json(ScreenReportResponse { wanted: screen.wanted() }).into_response()
}

/// `GET /api/v1/screen/feed`: WebSocket of [`ScreenFeedFrame`]s.
pub async fn feed(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| serve(state, socket))
}

async fn serve(state: AppState, mut socket: WebSocket) {
    let screen = &state.inner.screen;
    let _listening = screen.listen();
    tracing::info!("screen: a listener connected");
    let mut frames = screen.subscribe();
    loop {
        let text = {
            let frame = frames.borrow_and_update();
            serde_json::to_string(&*frame).unwrap_or_default()
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
        // Wait for the next change. Listeners send nothing; anything they do
        // send is ignored, and a close ends the stream.
        let open = loop {
            tokio::select! {
                changed = frames.changed() => break changed.is_ok(),
                message = socket.recv() => match message {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break false,
                    Some(Ok(_)) => {}
                },
            }
        };
        if !open {
            break;
        }
    }
    tracing::info!("screen: a listener left");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swatch(rgb: [u8; 3]) -> Option<Option<Lab>> {
        Some(Some(palette::from_srgb(rgb)))
    }

    fn color(tracker: &Tracker) -> Option<String> {
        let ScreenFeedFrame::ScreenColors { color, .. } = tracker.frame();
        color
    }

    const RED: [u8; 3] = [200, 40, 30];
    const BLUE: [u8; 3] = [30, 60, 200];

    #[test]
    fn first_picture_goes_out_at_once_then_changes_must_hold() {
        let t0 = Instant::now();
        let mut tracker = Tracker::default();
        assert!(tracker.report(true, Some("Dune".into()), None, t0));
        assert_eq!(color(&tracker), None);
        assert!(tracker.report(true, Some("Dune".into()), swatch(RED), t0));
        assert_eq!(color(&tracker).as_deref(), Some("#c8281e"));

        // A single blue frame (an explosion, a cut) changes nothing.
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(!tracker.report(true, Some("Dune".into()), swatch(BLUE), at(3000)));
        assert!(!tracker.report(true, Some("Dune".into()), swatch(RED), at(4000)));
        // Blue that holds goes out after HOLD.
        assert!(!tracker.report(true, Some("Dune".into()), swatch(BLUE), at(5000)));
        assert!(!tracker.report(true, Some("Dune".into()), swatch(BLUE), at(6000)));
        assert!(tracker.report(true, Some("Dune".into()), swatch(BLUE), at(6500)));
        assert_eq!(color(&tracker).as_deref(), Some("#1e3cc8"));
        // Nearly the same blue changes nothing.
        assert!(!tracker.report(true, Some("Dune".into()), swatch([32, 62, 204]), at(9000)));
    }

    #[test]
    fn changes_wait_for_the_gap() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut tracker = Tracker::default();
        tracker.report(true, None, swatch(RED), t0);
        assert!(!tracker.report(true, None, swatch(BLUE), at(100)));
        // Held for HOLD, but MIN_GAP since the last change hasn't passed.
        assert!(!tracker.report(true, None, swatch(BLUE), at(1700)));
        assert!(tracker.report(true, None, swatch(BLUE), at(2100)));
    }

    #[test]
    fn pause_and_resume() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut tracker = Tracker::default();
        tracker.report(true, Some("Dune".into()), swatch(RED), t0);
        assert!(tracker.report(false, Some("Dune".into()), swatch(BLUE), at(500)));
        let ScreenFeedFrame::ScreenColors { playing, .. } = tracker.frame();
        assert!(!playing);
        // Paused pictures are ignored; the colors stay.
        assert_eq!(color(&tracker).as_deref(), Some("#c8281e"));
        // After a resume, the first picture goes out at once.
        assert!(tracker.report(true, Some("Dune".into()), None, at(600)));
        assert!(tracker.report(true, Some("Dune".into()), swatch(BLUE), at(700)));
        assert_eq!(color(&tracker).as_deref(), Some("#1e3cc8"));
    }

    #[test]
    fn dark_scenes_must_last_too() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut tracker = Tracker::default();
        tracker.report(true, None, swatch(RED), t0);
        assert!(!tracker.report(true, None, Some(None), at(3000)));
        assert!(!tracker.report(true, None, swatch(RED), at(3500)));
        assert!(!tracker.report(true, None, Some(None), at(4000)));
        assert!(tracker.report(true, None, Some(None), at(5600)));
        assert_eq!(color(&tracker), None);
    }

    #[test]
    fn silence_stops_playback() {
        let t0 = Instant::now();
        let mut tracker = Tracker::default();
        assert!(!tracker.expire(t0 + SILENCE));
        tracker.report(true, Some("Dune".into()), None, t0);
        assert!(!tracker.expire(t0 + SILENCE / 2));
        assert!(tracker.expire(t0 + SILENCE));
        assert_eq!(
            tracker.frame(),
            ScreenFeedFrame::ScreenColors { playing: false, title: None, color: None }
        );
        assert!(!tracker.expire(t0 + SILENCE * 2));
    }

    #[test]
    fn rejects_bad_samples() {
        let sample = |width, height, rgb: &[u8]| ScreenSample {
            width,
            height,
            rgb: base64::engine::general_purpose::STANDARD.encode(rgb),
        };
        assert!(decode(&sample(2, 1, &[1, 2, 3, 4, 5, 6])).is_ok());
        assert!(decode(&sample(2, 1, &[1, 2, 3])).is_err());
        assert!(decode(&sample(0, 0, &[])).is_err());
        assert!(decode(&sample(100, 100, &[0; 30_000])).is_err());
        assert!(decode(&ScreenSample { width: 1, height: 1, rgb: "!!".into() }).is_err());
    }
}
