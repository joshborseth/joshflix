//! libmpv playback for Joshflix.
//!
//! [`Player`] owns an mpv instance, an event thread and a control thread that
//! runs every blocking mpv call, so callers (the UI thread) never wait on mpv.
//!
//! Video reaches the screen one of two ways:
//! - [`VideoRenderer`] draws mpv's frames into the UI's OpenGL context (call it
//!   from the UI toolkit's "before rendering" hook), so the player overlay is
//!   ordinary UI drawn on top. Desktop, and Android when it can't layer surfaces.
//! - On Android, mpv can own a separate surface under a translucent UI surface
//!   ([`PlayerConfig::android_surface`], [`Player::attach_android_surface`]) and
//!   draw there on its own threads with `vo=gpu`, as mpv-android does.
//!
//! On Android, mpv decodes with MediaCodec, copying frames back
//! (`mediacodec-copy`) or handing them to OpenGL through AImageReader without a
//! copy (`mediacodec`).

mod ffi;

#[cfg(target_os = "android")]
pub mod android;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use thiserror::Error;

use ffi::*;

#[derive(Debug, Error)]
pub enum PlayerError {
    #[error("The video player isn't available: {0}")]
    Unavailable(String),
    #[error("mpv error in {call}: {message}")]
    Mpv { call: &'static str, message: String },
}

pub type Result<T> = std::result::Result<T, PlayerError>;

/// Platform-specific tuning.
#[derive(Debug, Clone)]
pub struct PlayerConfig {
    /// `mediacodec` on Android, `auto-safe` elsewhere.
    pub hwdec: String,
    /// Audio outputs in preference order (`audiotrack,opensles` on Android).
    pub audio_output: Option<String>,
    /// Demuxer cache in MiB (TVs have little RAM).
    pub cache_mib: u32,
    pub user_agent: String,
    /// Directory for mpv's config/cache, if any.
    pub config_dir: Option<String>,
    /// Android audio session for the AudioTrack output, so the app can attach
    /// audio effects to it (Audio Normalization). `None` lets AudioTrack pick.
    pub audio_session: Option<i32>,
    /// Android: mpv draws into its own surface (given later with
    /// [`Player::attach_android_surface`]) instead of through [`VideoRenderer`].
    pub android_surface: bool,
}

impl PlayerConfig {
    pub fn for_platform() -> Self {
        let android = cfg!(target_os = "android");
        Self {
            hwdec: std::env::var("JOSHFLIX_HWDEC").unwrap_or_else(|_| {
                if android { "mediacodec,mediacodec-copy".into() } else { "auto-safe".into() }
            }),
            audio_output: android.then(|| "audiotrack,opensles".into()),
            cache_mib: if android { 96 } else { 256 },
            user_agent: concat!("Joshflix/", env!("CARGO_PKG_VERSION")).into(),
            config_dir: None,
            audio_session: None,
            android_surface: false,
        }
    }
}

/// One audio or subtitle track.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: i64,
    pub kind: TrackKind,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub codec: Option<String>,
    pub channels: Option<String>,
    pub selected: bool,
    pub external: bool,
    pub default: bool,
    pub forced: bool,
}

impl Track {
    /// "English · TrueHD 7.1" style label.
    pub fn label(&self) -> String {
        let lang = self.lang.as_deref().map(language_label);
        let mut parts: Vec<String> = Vec::new();
        match (&self.title, lang) {
            (Some(title), Some(lang)) if !title.to_lowercase().contains(&lang.to_lowercase()) => {
                parts.push(format!("{lang} — {title}"))
            }
            (Some(title), _) => parts.push(title.clone()),
            (None, Some(lang)) => parts.push(lang.to_owned()),
            (None, None) => parts.push(format!("Track {}", self.id)),
        }
        if let Some(codec) = self.codec.as_ref().filter(|_| self.kind != TrackKind::Subtitle) {
            let codec = codec.to_uppercase().replace("EAC3", "DD+").replace("AC3", "DD");
            parts.push(match &self.channels {
                Some(ch) => format!("{codec} {ch}"),
                None => codec,
            });
        }
        if self.forced {
            parts.push("Forced".into());
        }
        if self.external && !self.title.as_deref().is_some_and(|t| t.contains("OpenSubtitles")) {
            parts.push("OpenSubtitles".into());
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// Why playback ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    Eof,
    Stopped,
    Error(String),
}

/// Events from the player thread.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    FileLoaded,
    /// Playback (re)started after load or seek.
    Started,
    Position(f64),
    Duration(f64),
    Paused(bool),
    /// Buffering; `Some(percent)` while waiting for cache.
    Buffering(Option<i64>),
    TracksChanged,
    /// Container or estimated frame rate.
    FrameRate(f64),
    VideoSize { width: i64, height: i64 },
    /// Seconds of media buffered ahead of the playhead.
    CacheAhead(f64),
    /// The video output shows video (`video-out-params` exist), or stopped.
    VideoOutput(bool),
    /// Running totals for this file: frames dropped by the video output, and
    /// by the decoder.
    DroppedByOutput(i64),
    DroppedByDecoder(i64),
    Ended(EndReason),
    Log { level: String, text: String },
}

// Property observer IDs.
const OBS_TIME: u64 = 1;
const OBS_DURATION: u64 = 2;
const OBS_PAUSE: u64 = 3;
const OBS_PAUSED_FOR_CACHE: u64 = 4;
const OBS_CACHE_STATE: u64 = 5;
const OBS_TRACK_COUNT: u64 = 6;
const OBS_FPS: u64 = 7;
const OBS_WIDTH: u64 = 8;
const OBS_HEIGHT: u64 = 9;
const OBS_EOF: u64 = 10;
const OBS_SELECTION: u64 = 11;
const OBS_CACHE_AHEAD: u64 = 12;
const OBS_VIDEO_OUT: u64 = 13;
const OBS_OUTPUT_DROPS: u64 = 14;
const OBS_DECODER_DROPS: u64 = 15;

struct Inner {
    mpv: Mpv,
    handle: *mut mpv_handle,
    shutting_down: AtomicBool,
}

// SAFETY: the mpv client API is thread-safe for one handle (documented in client.h).
unsafe impl Send for Inner {}
// SAFETY: as above.
unsafe impl Sync for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: handle came from mpv_create and is destroyed once.
        unsafe { (self.mpv.mpv_terminate_destroy)(self.handle) };
    }
}

/// Work for the control thread. Every mpv call that can block runs there, so
/// the UI/render thread never waits on mpv's core (which can itself be waiting
/// for the render thread to consume MediaCodec frames).
enum Cmd {
    Command(Vec<String>),
    SetString(String, String),
    SetFlag(String, bool),
    RefreshTracks,
    Quit,
}

type EventFn = Arc<dyn Fn(PlayerEvent) + Send + Sync>;

/// The mpv instance. Cloning shares it.
#[derive(Clone)]
pub struct Player {
    inner: Arc<Inner>,
    commands: std::sync::mpsc::Sender<Cmd>,
    tracks: Arc<std::sync::Mutex<Vec<Track>>>,
    threads: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
}

fn cstring(value: &str) -> CString {
    CString::new(value.replace('\0', "")).unwrap_or_default()
}

impl Inner {
    fn check(&self, call: &'static str, code: c_int) -> Result<()> {
        if code < 0 {
            Err(PlayerError::Mpv { call, message: self.mpv.error_string(code) })
        } else {
            Ok(())
        }
    }

    fn set_option(&self, name: &str, value: &str) {
        let (n, v) = (cstring(name), cstring(value));
        // SAFETY: valid handle and strings.
        let code = unsafe { (self.mpv.mpv_set_option_string)(self.handle, n.as_ptr(), v.as_ptr()) };
        if code < 0 {
            tracing::warn!("mpv option {name}={value}: {}", self.mpv.error_string(code));
        }
    }

    fn command(&self, args: &[String]) -> Result<()> {
        let owned: Vec<CString> = args.iter().map(|a| cstring(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        // SAFETY: NULL-terminated array of valid C strings that outlive the call.
        let code = unsafe { (self.mpv.mpv_command)(self.handle, ptrs.as_mut_ptr()) };
        self.check("mpv_command", code)
    }

    fn set_string(&self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (cstring(name), cstring(value));
        // SAFETY: valid handle and strings.
        let code = unsafe { (self.mpv.mpv_set_property_string)(self.handle, n.as_ptr(), v.as_ptr()) };
        self.check("mpv_set_property_string", code)
    }

    fn set_flag(&self, name: &str, value: bool) -> Result<()> {
        let n = cstring(name);
        let mut flag: c_int = value.into();
        // SAFETY: MPV_FORMAT_FLAG takes a pointer to int.
        let code = unsafe { (self.mpv.mpv_set_property)(self.handle, n.as_ptr(), MPV_FORMAT_FLAG, (&raw mut flag).cast()) };
        self.check("mpv_set_property", code)
    }

    fn get_string(&self, name: &str) -> Option<String> {
        let n = cstring(name);
        // SAFETY: valid handle; result freed with mpv_free.
        unsafe {
            let raw = (self.mpv.mpv_get_property_string)(self.handle, n.as_ptr());
            if raw.is_null() {
                return None;
            }
            let value = CStr::from_ptr(raw).to_string_lossy().into_owned();
            (self.mpv.mpv_free)(raw.cast());
            Some(value)
        }
    }

    fn get_i64(&self, name: &str) -> Option<i64> {
        let n = cstring(name);
        let mut value = 0i64;
        // SAFETY: MPV_FORMAT_INT64 writes an i64.
        let code = unsafe { (self.mpv.mpv_get_property)(self.handle, n.as_ptr(), MPV_FORMAT_INT64, (&raw mut value).cast()) };
        (code >= 0).then_some(value)
    }

    fn get_bool(&self, name: &str) -> Option<bool> {
        let n = cstring(name);
        let mut value: c_int = 0;
        // SAFETY: MPV_FORMAT_FLAG writes an int.
        let code = unsafe { (self.mpv.mpv_get_property)(self.handle, n.as_ptr(), MPV_FORMAT_FLAG, (&raw mut value).cast()) };
        (code >= 0).then_some(value != 0)
    }

    fn read_tracks(&self) -> Vec<Track> {
        let count = self.get_i64("track-list/count").unwrap_or(0);
        (0..count)
            .filter_map(|i| {
                let p = |field: &str| format!("track-list/{i}/{field}");
                let kind = match self.get_string(&p("type"))?.as_str() {
                    "video" => TrackKind::Video,
                    "audio" => TrackKind::Audio,
                    "sub" => TrackKind::Subtitle,
                    _ => return None,
                };
                Some(Track {
                    id: self.get_i64(&p("id"))?,
                    kind,
                    title: self.get_string(&p("title")).filter(|t| !t.is_empty()),
                    lang: self.get_string(&p("lang")).filter(|t| !t.is_empty()),
                    codec: self.get_string(&p("codec")).filter(|t| !t.is_empty()),
                    channels: self.get_i64(&p("demux-channel-count")).map(|n| match n {
                        1 => "1.0".to_owned(),
                        2 => "2.0".to_owned(),
                        6 => "5.1".to_owned(),
                        8 => "7.1".to_owned(),
                        n => format!("{n}ch"),
                    }),
                    selected: self.get_bool(&p("selected")).unwrap_or(false),
                    external: self.get_bool(&p("external")).unwrap_or(false),
                    default: self.get_bool(&p("default")).unwrap_or(false),
                    forced: self.get_bool(&p("forced")).unwrap_or(false),
                })
            })
            .collect()
    }
}

fn control_loop(inner: &Inner, rx: std::sync::mpsc::Receiver<Cmd>, tracks: &std::sync::Mutex<Vec<Track>>, on_event: &EventFn) {
    while let Ok(cmd) = rx.recv() {
        let result = match cmd {
            Cmd::Command(args) => inner.command(&args),
            Cmd::SetString(name, value) => inner.set_string(&name, &value),
            Cmd::SetFlag(name, value) => inner.set_flag(&name, value),
            Cmd::RefreshTracks => {
                let fresh = inner.read_tracks();
                if let Ok(mut slot) = tracks.lock() {
                    *slot = fresh;
                }
                on_event(PlayerEvent::TracksChanged);
                Ok(())
            }
            Cmd::Quit => {
                let _ = inner.command(&["quit".to_owned()]);
                return;
            }
        };
        if let Err(e) = result {
            tracing::warn!("{e}");
        }
    }
}

impl Player {
    /// Create and initialize mpv. `on_event` runs on mpv's event or control thread.
    pub fn new(config: &PlayerConfig, on_event: impl Fn(PlayerEvent) + Send + Sync + 'static) -> Result<Self> {
        let mpv = Mpv::load().map_err(PlayerError::Unavailable)?;
        // SAFETY: plain constructor.
        let handle = unsafe { (mpv.mpv_create)() };
        if handle.is_null() {
            return Err(PlayerError::Unavailable("mpv_create failed".into()));
        }
        let inner = Arc::new(Inner { mpv, handle, shutting_down: AtomicBool::new(false) });

        let cache = format!("{}MiB", config.cache_mib);
        let back_cache = format!("{}MiB", config.cache_mib / 3);
        let mut options: Vec<(&str, &str)> = vec![
            // With its own surface, mpv outputs nothing until the surface arrives.
            ("vo", if config.android_surface { "null" } else { "libmpv" }),
            ("hwdec", &config.hwdec),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("osd-level", "0"),
            ("cache", "yes"),
            ("demuxer-max-bytes", &cache),
            ("demuxer-max-back-bytes", &back_cache),
            ("demuxer-readahead-secs", "30"),
            ("network-timeout", "30"),
            ("user-agent", &config.user_agent),
            ("sub-auto", "no"),
            ("sub-font-size", "46"),
            ("sub-border-size", "2.5"),
            ("sub-shadow-offset", "1.5"),
            ("sub-color", "#FFFFFFFF"),
            ("sub-margin-y", "48"),
            ("audio-file-auto", "no"),
            ("ytdl", "no"),
            ("load-scripts", "no"),
            ("config", "no"),
            ("save-position-on-quit", "no"),
            ("hr-seek", "default"),
            ("video-sync", "audio"),
            // Fast scaling on TV-class GPUs; mpv's defaults target desktop GPUs.
            ("profile", "fast"),
            ("tone-mapping", "auto"),
        ];
        if let Some(ao) = &config.audio_output {
            options.push(("ao", ao));
        }
        if config.android_surface {
            options.extend([("gpu-context", "android"), ("opengl-es", "yes"), ("force-window", "no")]);
        }
        let audio_session = config.audio_session.map(|id| id.to_string());
        if let Some(id) = &audio_session {
            options.push(("audiotrack-session-id", id));
        }
        for (name, value) in options {
            inner.set_option(name, value);
        }

        // SAFETY: handle is valid; initialize once.
        let code = unsafe { (inner.mpv.mpv_initialize)(inner.handle) };
        inner.check("mpv_initialize", code)?;

        let observe = [
            (OBS_TIME, "time-pos", MPV_FORMAT_DOUBLE),
            (OBS_DURATION, "duration", MPV_FORMAT_DOUBLE),
            (OBS_PAUSE, "pause", MPV_FORMAT_FLAG),
            (OBS_PAUSED_FOR_CACHE, "paused-for-cache", MPV_FORMAT_FLAG),
            (OBS_CACHE_STATE, "cache-buffering-state", MPV_FORMAT_INT64),
            (OBS_TRACK_COUNT, "track-list/count", MPV_FORMAT_INT64),
            (OBS_FPS, "container-fps", MPV_FORMAT_DOUBLE),
            (OBS_WIDTH, "video-params/w", MPV_FORMAT_INT64),
            (OBS_HEIGHT, "video-params/h", MPV_FORMAT_INT64),
            (OBS_EOF, "eof-reached", MPV_FORMAT_FLAG),
            (OBS_SELECTION, "sid", MPV_FORMAT_NONE),
            (OBS_SELECTION, "aid", MPV_FORMAT_NONE),
            (OBS_CACHE_AHEAD, "demuxer-cache-duration", MPV_FORMAT_DOUBLE),
            // Unlike vo-configured, this stays unset with force-window and no video.
            (OBS_VIDEO_OUT, "video-out-params/w", MPV_FORMAT_INT64),
            (OBS_OUTPUT_DROPS, "frame-drop-count", MPV_FORMAT_INT64),
            (OBS_DECODER_DROPS, "decoder-frame-drop-count", MPV_FORMAT_INT64),
        ];
        for (id, name, format) in observe {
            let name = cstring(name);
            // SAFETY: valid handle and NUL-terminated name.
            unsafe { (inner.mpv.mpv_observe_property)(inner.handle, id, name.as_ptr(), format) };
        }
        // Android logs at info (decoder, audio and video output choices) because
        // TVs are diagnosed from their uploaded logs. JOSHFLIX_MPV_LOG=v for more.
        let default_level = if cfg!(target_os = "android") { "info" } else { "warn" };
        let level = cstring(&std::env::var("JOSHFLIX_MPV_LOG").unwrap_or_else(|_| default_level.into()));
        // SAFETY: valid handle.
        unsafe { (inner.mpv.mpv_request_log_messages)(inner.handle, level.as_ptr()) };

        let on_event: EventFn = Arc::new(on_event);
        let (tx, rx) = std::sync::mpsc::channel();
        let tracks: Arc<std::sync::Mutex<Vec<Track>>> = Arc::default();
        let mut threads = Vec::new();
        {
            let (inner, tracks, on_event) = (inner.clone(), tracks.clone(), on_event.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("mpv-control".into())
                    .spawn(move || control_loop(&inner, rx, &tracks, &on_event))
                    .map_err(|e| PlayerError::Unavailable(e.to_string()))?,
            );
        }
        {
            let (inner, tx, on_event) = (inner.clone(), tx.clone(), on_event);
            threads.push(
                std::thread::Builder::new()
                    .name("mpv-events".into())
                    .spawn(move || event_loop(&inner, &tx, &*on_event))
                    .map_err(|e| PlayerError::Unavailable(e.to_string()))?,
            );
        }
        Ok(Self {
            inner,
            commands: tx,
            tracks,
            threads: Arc::new(std::sync::Mutex::new(threads)),
        })
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.commands
            .send(cmd)
            .map_err(|_| PlayerError::Unavailable("the player has shut down".into()))
    }

    /// Queue an mpv command (e.g. `["loadfile", url]`). Never blocks.
    pub fn command(&self, args: &[&str]) -> Result<()> {
        self.send(Cmd::Command(args.iter().map(|a| (*a).to_owned()).collect()))
    }

    /// Queue a property change. Never blocks.
    pub fn set_property(&self, name: &str, value: &str) -> Result<()> {
        self.send(Cmd::SetString(name.to_owned(), value.to_owned()))
    }

    pub fn set_flag(&self, name: &str, value: bool) -> Result<()> {
        self.send(Cmd::SetFlag(name.to_owned(), value))
    }

    // ---- high-level controls ----

    /// Load a URL, optionally starting at `start_secs`.
    pub fn load(&self, url: &str, start_secs: Option<f64>) -> Result<()> {
        let start = start_secs.filter(|s| *s > 1.0).map_or_else(|| "0".to_owned(), |s| format!("{s:.1}"));
        if let Ok(mut tracks) = self.tracks.lock() {
            tracks.clear();
        }
        self.set_property("start", &start)?;
        self.set_flag("pause", false)?;
        self.command(&["loadfile", url, "replace"])
    }

    pub fn stop(&self) -> Result<()> {
        self.command(&["stop"])
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.set_flag("pause", paused)
    }

    pub fn toggle_pause(&self) -> Result<()> {
        self.command(&["cycle", "pause"])
    }

    pub fn seek_to(&self, secs: f64) -> Result<()> {
        self.command(&["seek", &format!("{:.2}", secs.max(0.0)), "absolute"])
    }

    pub fn seek_by(&self, secs: f64) -> Result<()> {
        self.command(&["seek", &format!("{secs:.2}"), "relative"])
    }

    /// `None` disables the track type (subtitles off).
    pub fn select_track(&self, kind: TrackKind, id: Option<i64>) -> Result<()> {
        let property = match kind {
            TrackKind::Video => "vid",
            TrackKind::Audio => "aid",
            TrackKind::Subtitle => "sid",
        };
        self.set_property(property, &id.map_or_else(|| "no".to_owned(), |id| id.to_string()))
    }

    /// Add an external subtitle without selecting it.
    pub fn add_subtitle(&self, url: &str, title: &str, lang: &str) -> Result<()> {
        self.command(&["sub-add", url, "auto", title, lang])
    }

    pub fn set_subtitle_scale(&self, scale: f64) -> Result<()> {
        self.set_property("sub-scale", &format!("{:.2}", scale.clamp(0.5, 2.5)))
    }

    /// Audio Normalization through mpv's lavfi filters: compress dynamics so
    /// dialogue is audible and loud scenes are tamed. Needs an FFmpeg with
    /// audio filters, which Android's bundled libmpv lacks (the app uses a
    /// platform audio effect there).
    pub fn set_audio_normalization(&self, enabled: bool) -> Result<()> {
        let filter = if enabled {
            "lavfi=[acompressor=threshold=-24dB:ratio=4:attack=10:release=250:makeup=6dB,alimiter=limit=0.9]"
        } else {
            ""
        };
        self.set_property("af", filter)
    }

    /// Both handles refer to the same mpv instance.
    pub fn same_instance(&self, other: &Player) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// The current file's tracks, as of the last change mpv reported. Never blocks.
    pub fn tracks(&self) -> Vec<Track> {
        self.tracks.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Android: start drawing video into `surface`, a JNI global reference to
    /// an `android.view.Surface` that stays valid until
    /// [`Player::detach_android_surface`] returns. Blocks while mpv sets up its
    /// video output, so call it off the UI thread. Same steps as mpv-android.
    pub fn attach_android_surface(&self, surface: i64, width: i32, height: i32) -> Result<()> {
        let inner = &self.inner;
        let name = cstring("wid");
        let mut wid = surface;
        // SAFETY: "wid" takes MPV_FORMAT_INT64; the pointer is valid for the call.
        let code = unsafe { (inner.mpv.mpv_set_property)(inner.handle, name.as_ptr(), MPV_FORMAT_INT64, (&raw mut wid).cast()) };
        inner.check("mpv_set_property(wid)", code)?;
        inner.set_string("android-surface-size", &format!("{width}x{height}"))?;
        // Draw (black, subtitles) even before a video frame arrives.
        inner.set_string("force-window", "yes")?;
        inner.set_string("vo", "gpu")
    }

    /// Android: the surface changed size.
    pub fn set_android_surface_size(&self, width: i32, height: i32) -> Result<()> {
        self.set_property("android-surface-size", &format!("{width}x{height}"))
    }

    /// Android: stop using the surface. Blocks until mpv has let go of it.
    pub fn detach_android_surface(&self) -> Result<()> {
        let inner = &self.inner;
        inner.set_string("vo", "null")?;
        inner.set_string("force-window", "no")?;
        let name = cstring("wid");
        let mut wid = 0i64;
        // SAFETY: as in attach_android_surface.
        let code = unsafe { (inner.mpv.mpv_set_property)(inner.handle, name.as_ptr(), MPV_FORMAT_INT64, (&raw mut wid).cast()) };
        inner.check("mpv_set_property(wid)", code)
    }

    /// Stop the threads and destroy mpv once the last clone drops.
    pub fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.send(Cmd::Quit);
        // SAFETY: wakes mpv_wait_event so the event thread can exit.
        unsafe { (self.inner.mpv.mpv_wakeup)(self.inner.handle) };
        if let Ok(mut threads) = self.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
}

fn event_loop(inner: &Inner, control: &std::sync::mpsc::Sender<Cmd>, on_event: &(dyn Fn(PlayerEvent) + Send + Sync)) {
    loop {
        // SAFETY: valid handle; the returned event is valid until the next call.
        let event = unsafe { &*(inner.mpv.mpv_wait_event)(inner.handle, 1.0) };
        match event.event_id {
            MPV_EVENT_NONE => {
                if inner.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
            }
            MPV_EVENT_SHUTDOWN => return,
            MPV_EVENT_FILE_LOADED => on_event(PlayerEvent::FileLoaded),
            MPV_EVENT_PLAYBACK_RESTART => on_event(PlayerEvent::Started),
            MPV_EVENT_END_FILE => {
                // SAFETY: END_FILE carries mpv_event_end_file.
                let data = unsafe { &*(event.data as *const mpv_event_end_file) };
                let reason = match data.reason {
                    MPV_END_FILE_REASON_EOF => EndReason::Eof,
                    MPV_END_FILE_REASON_ERROR => EndReason::Error(inner.mpv.error_string(data.error)),
                    _ => EndReason::Stopped,
                };
                on_event(PlayerEvent::Ended(reason));
            }
            MPV_EVENT_LOG_MESSAGE => {
                // SAFETY: LOG_MESSAGE carries mpv_event_log_message with valid strings.
                let data = unsafe { &*(event.data as *const mpv_event_log_message) };
                let text = unsafe { CStr::from_ptr(data.text) }.to_string_lossy().trim_end().to_owned();
                let level = unsafe { CStr::from_ptr(data.level) }.to_string_lossy().into_owned();
                on_event(PlayerEvent::Log { level, text });
            }
            MPV_EVENT_PROPERTY_CHANGE => {
                // SAFETY: PROPERTY_CHANGE carries mpv_event_property.
                let prop = unsafe { &*(event.data as *const mpv_event_property) };
                // Track list or selection changed: re-read tracks off this thread.
                if event.reply_userdata == OBS_TRACK_COUNT || event.reply_userdata == OBS_SELECTION {
                    let _ = control.send(Cmd::RefreshTracks);
                    continue;
                }
                if event.reply_userdata == OBS_VIDEO_OUT {
                    let shown = prop.format == MPV_FORMAT_INT64
                        && !prop.data.is_null()
                        // SAFETY: MPV_FORMAT_INT64 points at an i64.
                        && unsafe { *(prop.data as *const i64) } > 0;
                    on_event(PlayerEvent::VideoOutput(shown));
                    continue;
                }
                if prop.data.is_null() {
                    continue;
                }
                // SAFETY: the format tells us the pointee type.
                let (double, int, flag) = unsafe {
                    match prop.format {
                        MPV_FORMAT_DOUBLE => (Some(*(prop.data as *const f64)), None, None),
                        MPV_FORMAT_INT64 => (None, Some(*(prop.data as *const i64)), None),
                        MPV_FORMAT_FLAG => (None, None, Some(*(prop.data as *const c_int) != 0)),
                        _ => (None, None, None),
                    }
                };
                let event = match (event.reply_userdata, double, int, flag) {
                    (OBS_TIME, Some(v), _, _) => PlayerEvent::Position(v),
                    (OBS_DURATION, Some(v), _, _) => PlayerEvent::Duration(v),
                    (OBS_PAUSE, _, _, Some(v)) => PlayerEvent::Paused(v),
                    (OBS_PAUSED_FOR_CACHE, _, _, Some(true)) => PlayerEvent::Buffering(Some(0)),
                    (OBS_PAUSED_FOR_CACHE, _, _, Some(false)) => PlayerEvent::Buffering(None),
                    (OBS_CACHE_STATE, _, Some(v), _) if v < 100 => PlayerEvent::Buffering(Some(v)),
                    (OBS_FPS, Some(v), _, _) => PlayerEvent::FrameRate(v),
                    (OBS_CACHE_AHEAD, Some(v), _, _) => PlayerEvent::CacheAhead(v),
                    (OBS_WIDTH, _, Some(w), _) => PlayerEvent::VideoSize { width: w, height: 0 },
                    (OBS_HEIGHT, _, Some(h), _) => PlayerEvent::VideoSize { width: 0, height: h },
                    (OBS_EOF, _, _, Some(true)) => PlayerEvent::Ended(EndReason::Eof),
                    (OBS_OUTPUT_DROPS, _, Some(n), _) => PlayerEvent::DroppedByOutput(n),
                    (OBS_DECODER_DROPS, _, Some(n), _) => PlayerEvent::DroppedByDecoder(n),
                    _ => continue,
                };
                on_event(event);
            }
            _ => {}
        }
    }
}

/// Draws mpv frames with OpenGL. Create it on the UI thread with the GL context current.
pub struct VideoRenderer {
    player: Player,
    ctx: *mut mpv_render_context,
    update_cb: *mut Box<dyn Fn() + Send + Sync>,
}

struct ProcAddress<'a>(&'a dyn Fn(&CStr) -> *const c_void);

unsafe extern "C" fn get_proc_address_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: ctx points at a live ProcAddress for the duration of mpv_render_context_create.
    unsafe {
        let lookup = &*(ctx as *const ProcAddress);
        (lookup.0)(CStr::from_ptr(name)) as *mut c_void
    }
}

unsafe extern "C" fn update_trampoline(ctx: *mut c_void) {
    // SAFETY: ctx is the boxed callback owned by the VideoRenderer, freed after mpv stops calling it.
    unsafe { (*(ctx as *const Box<dyn Fn() + Send + Sync>))() }
}

impl VideoRenderer {
    /// # Safety
    /// Must be called with the target OpenGL context current, on the thread that will render.
    /// `get_proc_address` must resolve GL functions for that context.
    pub unsafe fn new(
        player: &Player,
        get_proc_address: &dyn Fn(&CStr) -> *const c_void,
        on_new_frame: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let lookup = ProcAddress(get_proc_address);
        let mut init = mpv_opengl_init_params {
            get_proc_address: get_proc_address_trampoline,
            get_proc_address_ctx: (&raw const lookup) as *mut c_void,
        };
        let api = cstring("opengl");
        // No MPV_RENDER_PARAM_ADVANCED_CONTROL: with it, mpv's core may block on
        // the render thread, and our render thread is the UI thread, which also
        // makes synchronous mpv calls (deadlock with MediaCodec on Android).
        // Without it mpv preloads its hwdec interop here instead.
        let mut params = [
            mpv_render_param { type_: MPV_RENDER_PARAM_API_TYPE, data: api.as_ptr() as *mut c_void },
            mpv_render_param { type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, data: (&raw mut init).cast() },
            mpv_render_param { type_: MPV_RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        let mut ctx: *mut mpv_render_context = ptr::null_mut();
        // SAFETY: params are valid for the call; the GL context is current per contract.
        let code = unsafe {
            (player.inner.mpv.mpv_render_context_create)(&raw mut ctx, player.inner.handle, params.as_mut_ptr())
        };
        player.inner.check("mpv_render_context_create", code)?;
        let update_cb = Box::into_raw(Box::new(on_new_frame));
        // SAFETY: ctx is valid; update_cb lives until Drop, where the callback is cleared first.
        unsafe {
            (player.inner.mpv.mpv_render_context_set_update_callback)(ctx, Some(update_trampoline), update_cb.cast());
        }
        Ok(Self {
            player: player.clone(),
            ctx,
            update_cb,
        })
    }

    /// Render the current frame into framebuffer `fbo` sized `width`×`height`.
    /// Returns `true` when mpv had a frame to draw.
    pub fn render(&self, fbo: i32, width: i32, height: i32) -> bool {
        let mpv = &self.player.inner.mpv;
        // SAFETY: ctx is valid and we're on the render thread with the context current.
        let flags = unsafe { (mpv.mpv_render_context_update)(self.ctx) };
        let _ = flags & MPV_RENDER_UPDATE_FRAME;
        let mut target = mpv_opengl_fbo { fbo, w: width, h: height, internal_format: 0 };
        let mut flip: c_int = 1;
        let mut params = [
            mpv_render_param { type_: MPV_RENDER_PARAM_OPENGL_FBO, data: (&raw mut target).cast() },
            mpv_render_param { type_: MPV_RENDER_PARAM_FLIP_Y, data: (&raw mut flip).cast() },
            mpv_render_param { type_: MPV_RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        // SAFETY: as above.
        let code = unsafe { (mpv.mpv_render_context_render)(self.ctx, params.as_mut_ptr()) };
        code >= 0
    }

    /// Tell mpv a frame was presented (improves frame timing).
    pub fn report_swap(&self) {
        // SAFETY: ctx is valid.
        unsafe { (self.player.inner.mpv.mpv_render_context_report_swap)(self.ctx) };
    }
}

impl Drop for VideoRenderer {
    fn drop(&mut self) {
        let mpv = &self.player.inner.mpv;
        // SAFETY: clear the callback, free the context (GL context must be current, per new()'s contract),
        // then free the boxed callback mpv no longer references.
        unsafe {
            (mpv.mpv_render_context_set_update_callback)(self.ctx, None, ptr::null_mut());
            (mpv.mpv_render_context_free)(self.ctx);
            drop(Box::from_raw(self.update_cb));
        }
    }
}

/// English name for an ISO 639 code mpv reports (`eng`, `en`, `spa`…).
pub fn language_label(code: &str) -> &str {
    match code.to_ascii_lowercase().as_str() {
        "en" | "eng" => "English",
        "es" | "spa" => "Spanish",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "pob" | "pt-br" => "Portuguese (Brazil)",
        "nl" | "dut" | "nld" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nob" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "pl" | "pol" => "Polish",
        "ru" | "rus" => "Russian",
        "uk" | "ukr" => "Ukrainian",
        "tr" | "tur" => "Turkish",
        "el" | "gre" | "ell" => "Greek",
        "he" | "heb" => "Hebrew",
        "ar" | "ara" => "Arabic",
        "hi" | "hin" => "Hindi",
        "ja" | "jpn" => "Japanese",
        "ko" | "kor" => "Korean",
        "zh" | "chi" | "zho" => "Chinese",
        "vi" | "vie" => "Vietnamese",
        "th" | "tha" => "Thai",
        "id" | "ind" => "Indonesian",
        "cs" | "cze" | "ces" => "Czech",
        "hu" | "hun" => "Hungarian",
        "ro" | "rum" | "ron" => "Romanian",
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_labels() {
        let track = Track {
            id: 2,
            kind: TrackKind::Audio,
            title: Some("Surround".into()),
            lang: Some("eng".into()),
            codec: Some("truehd".into()),
            channels: Some("7.1".into()),
            selected: true,
            external: false,
            default: true,
            forced: false,
        };
        assert_eq!(track.label(), "English — Surround · TRUEHD 7.1");
        let sub = Track { kind: TrackKind::Subtitle, title: None, codec: Some("subrip".into()), channels: None, external: true, ..track };
        assert_eq!(sub.label(), "English · OpenSubtitles");
    }

    /// Plays a generated clip headless when libmpv is installed (skips otherwise).
    #[test]
    fn plays_a_local_clip_headless() {
        let clip = std::env::temp_dir().join(format!("joshflix-player-test-{}.mkv", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24", "-f", "lavfi", "-i", "sine=frequency=440", "-t", "3", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-metadata:s:a:0", "language=eng"])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut config = PlayerConfig::for_platform();
        config.audio_output = Some("null".into());
        let player = match Player::new(&config, move |e| {
            let _ = tx.send(e);
        }) {
            Ok(p) => p,
            Err(PlayerError::Unavailable(reason)) => {
                eprintln!("{reason}; skipping");
                let _ = std::fs::remove_file(&clip);
                return;
            }
            Err(e) => panic!("{e}"),
        };
        // No render context: use the null video output so mpv decodes without a GPU.
        player.set_property("vo", "null").unwrap();
        player.load(clip.to_str().unwrap(), None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (mut loaded, mut ended, mut duration) = (false, false, 0.0);
        while std::time::Instant::now() < deadline && !ended {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(PlayerEvent::FileLoaded) => loaded = true,
                Ok(PlayerEvent::Duration(d)) => duration = d,
                Ok(PlayerEvent::Ended(EndReason::Eof)) => ended = true,
                _ => {}
            }
        }
        let tracks = player.tracks();
        player.shutdown();
        let _ = std::fs::remove_file(&clip);
        assert!(loaded, "file never loaded");
        assert!(ended, "never reached the end");
        assert!((duration - 3.0).abs() < 0.5, "duration {duration}");
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Audio && t.lang.as_deref() == Some("eng")));
    }
}
