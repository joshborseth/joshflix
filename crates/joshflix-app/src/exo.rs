//! Android: Media3 ExoPlayer through `com.joshflix.tv.ExoBridge` (JNI).
//!
//! ExoPlayer is the TV's player: MediaCodec decodes straight onto the TV's
//! video layer (no copy and no OpenGL work per frame), the TV's own decoders
//! handle Dolby Vision and HDR, and audio can pass through to the TV or a
//! soundbar. mpv stays as the fallback for files ExoPlayer can't play.
//!
//! Every call returns at once: `ExoBridge` posts the work to Android's main
//! thread, ExoPlayer's application thread. Its events come back through
//! `nativeEvent` on that thread, become [`PlayerEvent`]s, and go to the
//! callback given to [`Exo::new`], tagged with the instance id like mpv's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jdouble, jint, jlong};
use jni::{JNIEnv, NativeMethod};
use joshflix_player::{EndReason, PlayerError, PlayerEvent, Result, Track, TrackKind};

use crate::exo_tracks::parse_tracks;

const CLASS_NAME: &str = "com.joshflix.tv.ExoBridge";

// Event types from ExoBridge.EV_*.
const EV_LOADED: jint = 1;
const EV_STARTED: jint = 2;
const EV_PROGRESS: jint = 3;
const EV_CACHE: jint = 4;
const EV_PAUSED: jint = 5;
const EV_BUFFERING: jint = 6;
const EV_TRACKS: jint = 7;
const EV_FRAME_RATE: jint = 8;
const EV_VIDEO_SIZE: jint = 9;
const EV_DROPS: jint = 10;
const EV_ENDED: jint = 11;
const EV_ERROR: jint = 12;
/// `ExoBridge.ERR_NETWORK`: reopening the link may help.
const ERR_NETWORK: f64 = 1.0;

type EventFn = Arc<dyn Fn(u64, PlayerEvent) + Send + Sync>;

/// What the event callback needs to find from a bare instance id.
struct Shared {
    tracks: Mutex<Vec<Track>>,
    on_event: EventFn,
}

fn instances() -> MutexGuard<'static, HashMap<u64, Arc<Shared>>> {
    static INSTANCES: OnceLock<Mutex<HashMap<u64, Arc<Shared>>>> = OnceLock::new();
    INSTANCES.get_or_init(Mutex::default).lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `ExoBridge`'s class, loaded through the app's class loader, with its
/// native methods registered.
static CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// One ExoPlayer. Cloning shares it; the last clone releases it.
#[derive(Clone)]
pub struct Exo {
    inner: Arc<Inner>,
}

struct Inner {
    id: u64,
    bridge: GlobalRef,
    shared: Arc<Shared>,
    audio_language: Mutex<Option<String>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        instances().remove(&self.id);
        let _ = call(&self.bridge, "release", "()V", &[]);
    }
}

fn exo_error(e: impl std::fmt::Display) -> PlayerError {
    PlayerError::Unavailable(format!("ExoPlayer: {e}"))
}

/// Run `f` with a JNI environment on this thread, inside a local frame (a
/// native thread never frees local references on its own).
fn with_env<T>(f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<T>) -> Result<T> {
    let vm = crate::video_surface::vm().ok_or_else(|| exo_error("no Java VM"))?;
    let mut env = vm.attach_current_thread().map_err(exo_error)?;
    let result = env.with_local_frame(16, |env| f(env));
    result.map_err(|e| {
        if matches!(e, jni::errors::Error::JavaException) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        exo_error(e)
    })
}

fn call(bridge: &GlobalRef, name: &str, sig: &str, args: &[JValue]) -> Result<()> {
    with_env(|env| env.call_method(bridge.as_obj(), name, sig, args).map(|_| ()))
}

/// Load the bridge class and register its native methods (once per process).
fn bridge_class() -> Result<&'static GlobalRef> {
    if let Some(class) = CLASS.get() {
        return Ok(class);
    }
    let class = crate::video_surface::load_app_class(CLASS_NAME).map_err(exo_error)?;
    with_env(|env| {
        let methods = [
            NativeMethod {
                name: "nativeEvent".into(),
                sig: "(JIDDLjava/lang/String;)V".into(),
                fn_ptr: native_event as *mut std::ffi::c_void,
            },
            NativeMethod {
                name: "nativeLog".into(),
                sig: "(ILjava/lang/String;)V".into(),
                fn_ptr: native_log as *mut std::ffi::c_void,
            },
        ];
        env.register_native_methods(<&JClass>::from(class.as_obj()), &methods)
    })?;
    Ok(CLASS.get_or_init(|| class))
}

/// ExoPlayer can play on this TV: the bridge loads, video has its own layer,
/// and this version's ExoPlayer hasn't kept crashing the app.
pub fn available() -> bool {
    if !crate::video_surface::enabled() {
        return false;
    }
    if let Err(e) = bridge_class() {
        tracing::warn!("{e}");
        return false;
    }
    let Some(activity) = crate::video_surface::activity_class() else { return false };
    with_env(|env| env.call_static_method(<&JClass>::from(activity.as_obj()), "exoAllowed", "()Z", &[])?.z()).unwrap_or(false)
}

/// Show ExoPlayer's video layer (`true`) or mpv's.
pub fn show_layer(exo: bool) {
    let Some(activity) = crate::video_surface::activity_class() else { return };
    let result = with_env(|env| {
        env.call_static_method(<&JClass>::from(activity.as_obj()), "showVideoLayer", "(I)V", &[JValue::Int(if exo { 2 } else { 1 })])
            .map(|_| ())
    });
    if let Err(e) = result {
        tracing::warn!("video layer: {e}");
    }
}

impl Exo {
    /// A new ExoPlayer (created on first use) whose events go to `on_event`
    /// with `id`. `audio_session` is the session Audio Normalization's
    /// effect is attached to.
    pub fn new(
        id: u64,
        audio_session: Option<i32>,
        user_agent: &str,
        on_event: impl Fn(u64, PlayerEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let class = bridge_class()?;
        let activity = crate::video_surface::activity().ok_or_else(|| exo_error("no activity"))?;
        let bridge = with_env(|env| {
            let agent = env.new_string(user_agent)?;
            let object = env
                .call_static_method(
                    <&JClass>::from(class.as_obj()),
                    "create",
                    "(JLandroid/content/Context;ILjava/lang/String;)Lcom/joshflix/tv/ExoBridge;",
                    &[
                        JValue::Long(id as jlong),
                        JValue::Object(activity.as_obj()),
                        JValue::Int(audio_session.unwrap_or(0)),
                        JValue::Object(&agent),
                    ],
                )?
                .l()?;
            env.new_global_ref(object)
        })?;
        let shared = Arc::new(Shared { tracks: Mutex::default(), on_event: Arc::new(on_event) });
        instances().insert(id, shared.clone());
        Ok(Self { inner: Arc::new(Inner { id, bridge, shared, audio_language: Mutex::default() }) })
    }

    fn call(&self, name: &str, sig: &str, args: &[JValue]) -> Result<()> {
        call(&self.inner.bridge, name, sig, args)
    }

    /// Open `url`, starting at `start_secs`.
    pub fn load(&self, url: &str, start_secs: Option<f64>) -> Result<()> {
        if let Ok(mut tracks) = self.inner.shared.tracks.lock() {
            tracks.clear();
        }
        let start_ms = start_secs.filter(|s| *s > 1.0).map_or(0, |s| (s * 1000.0) as i64);
        let language = self.inner.audio_language.lock().ok().and_then(|l| l.clone());
        with_env(|env| {
            let url = env.new_string(url)?;
            let language: JObject = match &language {
                Some(l) => env.new_string(l)?.into(),
                None => JObject::null(),
            };
            env.call_method(
                self.inner.bridge.as_obj(),
                "load",
                "(Ljava/lang/String;JLjava/lang/String;)V",
                &[JValue::Object(&url), JValue::Long(start_ms), JValue::Object(&language)],
            )
            .map(|_| ())
        })
    }

    pub fn stop(&self) -> Result<()> {
        self.call("stop", "()V", &[])
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.call("setPaused", "(Z)V", &[JValue::Bool(paused.into())])
    }

    pub fn toggle_pause(&self) -> Result<()> {
        self.call("togglePause", "()V", &[])
    }

    pub fn seek_to(&self, secs: f64) -> Result<()> {
        self.call("seekTo", "(J)V", &[JValue::Long((secs.max(0.0) * 1000.0) as i64)])
    }

    pub fn seek_by(&self, secs: f64) -> Result<()> {
        self.call("seekBy", "(J)V", &[JValue::Long((secs * 1000.0) as i64)])
    }

    /// `None` turns subtitles off.
    pub fn select_track(&self, kind: TrackKind, id: Option<i64>) -> Result<()> {
        let kind = match kind {
            TrackKind::Video => 0,
            TrackKind::Audio => 1,
            TrackKind::Subtitle => 2,
        };
        let id = id.and_then(|id| i32::try_from(id).ok()).unwrap_or(-1);
        self.call("selectTrack", "(II)V", &[JValue::Int(kind), JValue::Int(id)])
    }

    /// Download and add a subtitle file without selecting it.
    pub fn add_subtitle(&self, url: &str, title: &str, lang: &str) -> Result<()> {
        with_env(|env| {
            let (url, title, lang) = (env.new_string(url)?, env.new_string(title)?, env.new_string(lang)?);
            env.call_method(
                self.inner.bridge.as_obj(),
                "addSubtitle",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[JValue::Object(&url), JValue::Object(&title), JValue::Object(&lang)],
            )
            .map(|_| ())
        })
    }

    pub fn set_subtitle_scale(&self, scale: f64) -> Result<()> {
        self.call("setSubtitleScale", "(F)V", &[JValue::Float(scale.clamp(0.5, 2.5) as f32)])
    }

    pub fn set_subtitles_lifted(&self, lifted: bool) -> Result<()> {
        self.call("setSubtitlesLifted", "(Z)V", &[JValue::Bool(lifted.into())])
    }

    pub fn set_subtitles_hidden(&self, hidden: bool) -> Result<()> {
        self.call("setSubtitlesHidden", "(Z)V", &[JValue::Bool(hidden.into())])
    }

    /// The audio language to prefer from the next [`Exo::load`] on.
    pub fn set_audio_language(&self, language: Option<&str>) {
        if let Ok(mut slot) = self.inner.audio_language.lock() {
            *slot = language.filter(|l| !l.is_empty()).map(str::to_owned);
        }
    }

    /// Decode all audio to PCM instead of passing it through (Audio
    /// Normalization's effect only works on PCM).
    pub fn set_pcm_only(&self, on: bool) -> Result<()> {
        self.call("setPcmOnly", "(Z)V", &[JValue::Bool(on.into())])
    }

    /// The current file's tracks, as of the last change. Never blocks on ExoPlayer.
    pub fn tracks(&self) -> Vec<Track> {
        self.inner.shared.tracks.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

fn java_string(env: &mut JNIEnv, text: &JString) -> Option<String> {
    if text.is_null() {
        return None;
    }
    env.get_string(text).ok().map(Into::into)
}

/// `ExoBridge.nativeEvent`, on Android's main thread.
extern "system" fn native_event(mut env: JNIEnv, _class: JClass, handle: jlong, kind: jint, a: jdouble, b: jdouble, text: JString) {
    let id = handle as u64;
    let Some(shared) = instances().get(&id).cloned() else { return };
    let emit = |event: PlayerEvent| (shared.on_event)(id, event);
    match kind {
        EV_LOADED => emit(PlayerEvent::FileLoaded),
        EV_STARTED => emit(PlayerEvent::Started),
        EV_PROGRESS => {
            emit(PlayerEvent::Position(a));
            if b > 0.0 {
                emit(PlayerEvent::Duration(b));
            }
        }
        EV_CACHE => emit(PlayerEvent::CacheAhead(a)),
        EV_PAUSED => emit(PlayerEvent::Paused(a != 0.0)),
        EV_BUFFERING => emit(PlayerEvent::Buffering((a >= 0.0).then_some(a as i64))),
        EV_TRACKS => {
            let Some(json) = java_string(&mut env, &text) else { return };
            match parse_tracks(&json) {
                Ok(tracks) => {
                    if let Ok(mut slot) = shared.tracks.lock() {
                        *slot = tracks;
                    }
                    emit(PlayerEvent::TracksChanged);
                }
                Err(e) => tracing::warn!("exo: track list: {e}"),
            }
        }
        EV_FRAME_RATE => emit(PlayerEvent::FrameRate(a)),
        EV_VIDEO_SIZE => {
            let (width, height) = (a as i64, b as i64);
            emit(PlayerEvent::VideoSize { width, height });
            emit(PlayerEvent::DecodedWidth(width));
            emit(PlayerEvent::VideoOutputWidth(width));
        }
        EV_DROPS => {
            emit(PlayerEvent::DroppedByOutput(a as i64));
            emit(PlayerEvent::DroppedByDecoder(b as i64));
        }
        EV_ENDED => emit(PlayerEvent::Ended(EndReason::Eof)),
        EV_ERROR => {
            let message = java_string(&mut env, &text).unwrap_or_else(|| "unknown error".into());
            if a == ERR_NETWORK {
                emit(PlayerEvent::Ended(EndReason::Error(message)));
            } else {
                emit(PlayerEvent::EngineFailed(message));
            }
        }
        _ => {}
    }
}

/// `ExoBridge.nativeLog`, from any Java thread: into the TV log.
extern "system" fn native_log(mut env: JNIEnv, _class: JClass, level: jint, text: JString) {
    let Some(text) = java_string(&mut env, &text) else { return };
    match level {
        2 => tracing::error!("{text}"),
        1 => tracing::warn!("{text}"),
        _ => tracing::info!("{text}"),
    }
}
