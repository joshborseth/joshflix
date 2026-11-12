//! Screen colors for the house lights.
//!
//! The relay on the dev box streams the colors on screen to the lights app
//! (`crates/joshflix-relay/src/screen.rs`). This side tells it what plays and,
//! while it plays and the relay says someone listens, sends a tiny copy of the
//! picture about once a second. Nobody listening costs a small post every few
//! seconds and no picture reads.
//!
//! Everything runs on its own low-priority thread, with its own small tokio
//! runtime and HTTP client, so it never depends on the app runtime that
//! `run()` shuts down when the UI ends. The controller only records what
//! plays ([`set_now_playing`]), which never blocks. The picture comes from
//! `MainActivity.sampleScreen` (Android's PixelCopy, scaled on the GPU), so
//! video playback never waits for it.

// Only Android reads the picture and starts the reporter.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use joshflix_protocol::{
    SCREEN_PATH, SCREEN_SAMPLE_HEIGHT, SCREEN_SAMPLE_WIDTH, ScreenReport, ScreenReportResponse,
    ScreenSample,
};

/// How often a picture goes out while someone listens.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How often the relay hears from the TV in the player otherwise. Its
/// answer says when someone starts listening; the relay treats 20 s of
/// silence as stopped.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// Wait this long after a failed post (the relay is down or the TV is away).
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// What plays, as the player sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlaying {
    /// Video is playing: started, not paused, no error.
    pub playing: bool,
    /// What plays; `None` outside the player.
    pub title: Option<String>,
}

static NOW: Mutex<NowPlaying> = Mutex::new(NowPlaying { playing: false, title: None });
static CHANGED: Condvar = Condvar::new();

fn now_playing() -> MutexGuard<'static, NowPlaying> {
    NOW.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record what plays. Cheap when nothing changed; called on every player tick.
pub fn set_now_playing(playing: bool, title: Option<&str>) {
    let mut now = now_playing();
    if now.playing != playing || now.title.as_deref() != title {
        *now = NowPlaying { playing, title: title.map(str::to_owned) };
        CHANGED.notify_all();
    }
}

/// What the reporter should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Post now, with a picture or without.
    Post { sample: bool },
    /// Wait up to this long (or until what plays changes).
    Wait(Duration),
}

/// The reporter's memory of what it told the relay.
#[derive(Debug, Default)]
pub struct Reporter {
    /// What the relay last heard, if a post went through.
    sent: Option<NowPlaying>,
    last_post: Option<Instant>,
    /// The last post failed.
    failing: bool,
    /// The relay said someone listens.
    wanted: bool,
}

impl Reporter {
    pub fn next(&self, now: &NowPlaying, at: Instant) -> Step {
        let since = self.last_post.map(|t| at.duration_since(t));
        let wait_for = |every: Duration| match since {
            Some(since) if since < every => Step::Wait(every - since),
            _ => Step::Post { sample: now.playing && self.wanted },
        };
        if self.failing {
            return if self.sent.as_ref() != Some(now) || now.title.is_some() {
                wait_for(RETRY_AFTER)
            } else {
                Step::Wait(Duration::from_secs(60))
            };
        }
        if self.sent.as_ref() != Some(now) {
            return Step::Post { sample: now.playing && self.wanted };
        }
        if now.playing && self.wanted {
            wait_for(SAMPLE_EVERY)
        } else if now.title.is_some() {
            wait_for(HEARTBEAT_EVERY)
        } else {
            // Out of the player and the relay knows: nothing to say.
            Step::Wait(Duration::from_secs(60))
        }
    }

    /// Record a post of `now` at `at`: the relay's answer, or `None` when it failed.
    pub fn posted(&mut self, now: NowPlaying, at: Instant, answer: Option<ScreenReportResponse>) {
        self.last_post = Some(at);
        match answer {
            Some(answer) => {
                if answer.wanted != self.wanted {
                    tracing::info!(
                        "screen colors: {}",
                        if answer.wanted { "the lights are listening" } else { "nobody listens" }
                    );
                }
                self.wanted = answer.wanted;
                self.sent = Some(now);
                self.failing = false;
            }
            None => {
                self.failing = true;
                self.wanted = false;
            }
        }
    }
}

/// ARGB pixels (Android's `int` colors) as packed RGB bytes.
pub fn argb_to_rgb(pixels: &[i32]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|&p| {
            let p = p as u32;
            [(p >> 16) as u8, (p >> 8) as u8, p as u8]
        })
        .collect()
}

/// Start the reporter thread, once per process (the activity, and with it
/// `run()`, can start again in the same process). `sample` reads the picture
/// at the given size as packed RGB, or returns `None` when it can't right now.
pub fn spawn(server: &str, sample: impl Fn(u32, u32) -> Option<Vec<u8>> + Send + 'static) {
    use base64::Engine as _;

    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let url = format!("{}{SCREEN_PATH}", server.trim_end_matches('/'));
    let spawned = std::thread::Builder::new().name("screen-colors".into()).spawn(move || {
        crate::platform::lower_thread_priority();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let (rt, http) = match (runtime, joshflix_core::http::Http::new()) {
            (Ok(rt), Ok(http)) => (rt, http),
            (Err(e), _) => return tracing::warn!("screen colors: no runtime ({e})"),
            (_, Err(e)) => return tracing::warn!("screen colors: no HTTP client ({e})"),
        };
        let mut reporter = Reporter::default();
        let mut misses = 0u32;
        loop {
            let now = now_playing().clone();
            let sample_now = match reporter.next(&now, Instant::now()) {
                Step::Wait(wait) => {
                    let guard = now_playing();
                    // Wakes early when what plays changes.
                    let _ = CHANGED.wait_timeout_while(guard, wait, |current| *current == now);
                    continue;
                }
                Step::Post { sample } => sample,
            };
            let picture = if sample_now {
                let picture = sample(SCREEN_SAMPLE_WIDTH, SCREEN_SAMPLE_HEIGHT);
                // Say so once in a while, without flooding the TV log.
                misses = if picture.is_some() { 0 } else { misses + 1 };
                if misses == 3 || (misses > 0 && misses.is_multiple_of(300)) {
                    tracing::warn!("screen colors: can't read the picture ({misses} tries in a row)");
                }
                picture
            } else {
                None
            };
            let report = ScreenReport {
                playing: now.playing,
                title: now.title.clone(),
                sample: picture.map(|rgb| ScreenSample {
                    width: SCREEN_SAMPLE_WIDTH,
                    height: SCREEN_SAMPLE_HEIGHT,
                    rgb: base64::engine::general_purpose::STANDARD.encode(rgb),
                }),
            };
            let request = http.client().post(&url).json(&report).timeout(Duration::from_secs(5));
            let answer = rt.block_on(async move {
                let response = request.send().await.ok()?.error_for_status().ok()?;
                response.json::<ScreenReportResponse>().await.ok()
            });
            reporter.posted(now, Instant::now(), answer);
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("screen colors: no reporter thread ({e})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(title: &str) -> NowPlaying {
        NowPlaying { playing: true, title: Some(title.into()) }
    }

    const YES: Option<ScreenReportResponse> = Some(ScreenReportResponse { wanted: true });
    const NO: Option<ScreenReportResponse> = Some(ScreenReportResponse { wanted: false });

    #[test]
    fn posts_changes_at_once_and_pictures_only_when_wanted() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut r = Reporter::default();
        let idle = NowPlaying::default();
        assert_eq!(r.next(&idle, t0), Step::Post { sample: false });
        r.posted(idle.clone(), t0, NO);
        assert_eq!(r.next(&idle, at(100)), Step::Wait(Duration::from_secs(60)));

        let dune = playing("Dune");
        assert_eq!(r.next(&dune, at(200)), Step::Post { sample: false });
        r.posted(dune.clone(), at(200), NO);
        // Nobody listens: a heartbeat every few seconds.
        assert_eq!(r.next(&dune, at(1200)), Step::Wait(HEARTBEAT_EVERY - Duration::from_secs(1)));
        assert_eq!(r.next(&dune, at(5200)), Step::Post { sample: false });
        r.posted(dune.clone(), at(5200), YES);
        // Someone listens: a picture every second.
        assert_eq!(r.next(&dune, at(5700)), Step::Wait(Duration::from_millis(500)));
        assert_eq!(r.next(&dune, at(6200)), Step::Post { sample: true });

        // A pause goes out at once, without a picture.
        let paused = NowPlaying { playing: false, ..dune };
        assert_eq!(r.next(&paused, at(6300)), Step::Post { sample: false });
        r.posted(paused.clone(), at(6300), YES);
        assert_eq!(r.next(&paused, at(6400)), Step::Wait(Duration::from_millis(4900)));
    }

    #[test]
    fn backs_off_when_the_relay_is_away() {
        let t0 = Instant::now();
        let mut r = Reporter::default();
        let dune = playing("Dune");
        r.posted(dune.clone(), t0, YES);
        r.posted(dune.clone(), t0 + Duration::from_secs(1), None);
        assert_eq!(r.next(&dune, t0 + Duration::from_secs(2)), Step::Wait(Duration::from_secs(4)));
        // No pictures until the relay answers again.
        assert_eq!(r.next(&dune, t0 + Duration::from_secs(6)), Step::Post { sample: false });
        // Out of the player while the relay is away: still retried, then quiet.
        let idle = NowPlaying::default();
        assert_eq!(r.next(&idle, t0 + Duration::from_secs(6)), Step::Post { sample: false });
        r.posted(idle.clone(), t0 + Duration::from_secs(6), None);
        assert_eq!(r.next(&idle, t0 + Duration::from_secs(7)), Step::Wait(Duration::from_secs(4)));
    }

    #[test]
    fn converts_argb() {
        assert_eq!(argb_to_rgb(&[0xff_c8_28_1e_u32 as i32, 0x00_01_02_03]), [0xc8, 0x28, 0x1e, 1, 2, 3]);
    }
}
