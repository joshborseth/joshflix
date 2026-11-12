//! Android: mpv draws video into its own surface under the UI.
//!
//! `MainActivity` (android/java) stacks two `SurfaceView`s: mpv's video surface
//! at the bottom and the translucent UI surface on top, which NativeActivity
//! hands to the Rust UI as its window. Video never passes through the UI's
//! OpenGL context, so a slow or lost UI surface can't stall or stop it.
//!
//! mpv is attached only while the player wants video ([`want_video`]), so a
//! TV whose driver can't handle this still reaches every other screen,
//! including the updater. Attaching runs on a worker thread. Only
//! `surfaceDestroyed` (Android's main thread) waits for mpv, because the
//! surface must be released before it returns; every wait has a timeout in case
//! an instance is wedged.
//!
//! Safety nets, recorded per app version by `MainActivity`:
//! - until video has shown once in this process, a marker is written before
//!   each attach; it is removed once video shows or mpv lets go cleanly, so a
//!   crash on this path makes the next launch use a single surface;
//! - [`give_up`] (video never appeared) does the same, and the app switches to
//!   drawing video into the UI right away.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::jint;
use jni::{JNIEnv, JavaVM, NativeMethod};
use joshflix_player::Player;

/// How long attaching or detaching may block before the instance counts as wedged.
const MPV_TIMEOUT: Duration = Duration::from_secs(2);
/// How long startup waits for `MainActivity.onCreate` to finish setting up layers.
const LAYER_WAIT: Duration = Duration::from_secs(3);

struct Surface {
    object: GlobalRef,
    width: i32,
    height: i32,
}

#[derive(Default)]
struct State {
    /// This process's current `MainActivity` and its class.
    activity: Option<GlobalRef>,
    class: Option<GlobalRef>,
    surface: Option<Surface>,
    /// The instance that should show video.
    current: Option<Player>,
    /// The player screen is open.
    want_video: bool,
    /// The instance attached to `surface` (possibly still attaching).
    attached: Option<Player>,
    /// How the attach call for `attached` went (see [`ATTACHING`]).
    attach_status: Option<Arc<AtomicU8>>,
}

/// `State::attach_status` values.
const ATTACHING: u8 = 0;
const ATTACHED: u8 = 1;
const ATTACH_FAILED: u8 = 2;

static VM: OnceLock<JavaVM> = OnceLock::new();
static STATE: Mutex<State> = Mutex::new(State {
    activity: None,
    class: None,
    surface: None,
    current: None,
    want_video: false,
    attached: None,
    attach_status: None,
});
/// Held for every blocking mpv call, so the surface can't be destroyed under one.
static MPV_CALLS: Mutex<()> = Mutex::new(());
static WORKER: OnceLock<mpsc::Sender<()>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
static UI_SHOWN: AtomicBool = AtomicBool::new(false);
static VIDEO_WORKED: AtomicBool = AtomicBool::new(false);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mpv_calls() -> MutexGuard<'static, ()> {
    MPV_CALLS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// mpv has its own surface (set up by [`init`], until [`give_up`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Call at the start of every `android_main` (an activity can be created again
/// in the same process), before creating the player. Returns whether video gets
/// its own surface; otherwise it is drawn into the UI as on desktop.
pub fn init(vm: *mut c_void, activity: *mut c_void) -> bool {
    ENABLED.store(false, Ordering::Relaxed);
    UI_SHOWN.store(false, Ordering::Relaxed);
    VIDEO_WORKED.store(false, Ordering::Relaxed);
    // Nothing from an earlier activity in this process is valid now. Dropped
    // outside the lock: the last Player clone shuts its mpv instance down.
    let stale = {
        let mut state = state();
        if state.attached.is_some() {
            abandon(state.surface.take());
        }
        std::mem::take(&mut *state)
    };
    drop(stale);
    match try_init(vm, activity) {
        Ok(true) => {
            tracing::info!("video: own surface under the UI");
            true
        }
        Ok(false) => {
            tracing::info!("video: drawn into the UI surface (single surface)");
            false
        }
        Err(e) => {
            tracing::warn!("video: drawn into the UI surface ({e})");
            false
        }
    }
}

fn try_init(vm_ptr: *mut c_void, activity: *mut c_void) -> Result<bool, String> {
    if VM.get().is_none() {
        // SAFETY: android-activity hands us a valid JavaVM pointer for the process lifetime.
        let vm = unsafe { JavaVM::from_raw(vm_ptr.cast()) }.map_err(|e| e.to_string())?;
        let _ = VM.set(vm);
    }
    let vm = VM.get().ok_or("no JavaVM")?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    // SAFETY: android-activity's activity reference is valid while this activity lives.
    let activity = unsafe { JObject::from_raw(activity.cast()) };
    let class = load_activity_class(&mut env, &activity).map_err(|e| describe(&mut env, e))?;
    let Some(class) = class else { return Ok(false) };
    {
        let class_ref = env.new_global_ref(&class).map_err(|e| e.to_string())?;
        let activity_ref = env.new_global_ref(&activity).map_err(|e| e.to_string())?;
        let mut state = state();
        state.class = Some(class_ref);
        state.activity = Some(activity_ref);
    }

    let started = Instant::now();
    let layered = loop {
        let layers = env
            .call_static_method(&class, "layerState", "()I", &[])
            .and_then(|v| v.i())
            .map_err(|e| describe(&mut env, e))?;
        match layers {
            1 => break true,
            0 if started.elapsed() < LAYER_WAIT => std::thread::sleep(Duration::from_millis(20)),
            _ => break false,
        }
    };
    if !layered {
        return Ok(false);
    }

    let method = NativeMethod {
        name: "nativeVideoSurface".into(),
        sig: "(Landroid/view/Surface;II)V".into(),
        fn_ptr: native_video_surface as *mut c_void,
    };
    env.register_native_methods(&class, &[method]).map_err(|e| describe(&mut env, e))?;
    if WORKER.get().is_none() {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("video-surface".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    reconcile();
                }
            })
            .map_err(|e| e.to_string())?;
        let _ = WORKER.set(tx);
    }
    ENABLED.store(true, Ordering::Relaxed);
    // The callback may run right away, on this thread, with the current surface.
    if let Err(e) = env.call_static_method(&class, "startVideoSurfaceUpdates", "()V", &[]) {
        ENABLED.store(false, Ordering::Relaxed);
        return Err(describe(&mut env, e));
    }
    Ok(true)
}

/// `com.joshflix.tv.MainActivity` through the activity's class loader (the
/// system loader on a native thread can't see app classes), or `None` when the
/// APK was built without it.
fn load_activity_class<'a>(env: &mut JNIEnv<'a>, activity: &JObject) -> jni::errors::Result<Option<JClass<'a>>> {
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string("com.joshflix.tv.MainActivity")?;
    match env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)]) {
        Ok(class) => Ok(Some(JClass::from(class.l()?))),
        Err(jni::errors::Error::JavaException) => {
            env.exception_clear()?;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Turn a pending Java exception into text and clear it.
fn describe(env: &mut JNIEnv, e: jni::errors::Error) -> String {
    if matches!(e, jni::errors::Error::JavaException) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    e.to_string()
}

/// Call a static `()V` method on `MainActivity`, from any thread.
fn call_activity_static(name: &str) {
    let Some(class) = state().class.clone() else { return };
    let Some(vm) = VM.get() else { return };
    let result = vm.attach_current_thread().map_err(|e| e.to_string()).and_then(|mut env| {
        let class = <&JClass>::from(class.as_obj());
        env.call_static_method(class, name, "()V", &[]).map(|_| ()).map_err(|e| describe(&mut env, e))
    });
    if let Err(e) = result {
        tracing::error!("MainActivity.{name}: {e}");
    }
}

/// Call after each UI frame. The first one tells `MainActivity` that the UI
/// surface works; without that it gives up on layered surfaces after 15 s.
pub fn ui_shown() {
    if !UI_SHOWN.swap(true, Ordering::Relaxed) {
        call_activity_static("uiShown");
        tracing::info!("first UI frame shown");
    }
}

/// Make `player` the instance that shows video.
pub fn set_player(player: Player) {
    state().current = Some(player);
    wake();
}

/// The player screen opened (`true`) or closed. mpv holds the surface only
/// while it's open.
pub fn want_video(on: bool) {
    state().want_video = on;
    wake();
}

/// `player` finished attaching to the video surface.
pub fn is_attached(player: &Player) -> bool {
    let state = state();
    state.attached.as_ref().is_some_and(|p| p.same_instance(player))
        && state.attach_status.as_ref().is_some_and(|s| s.load(Ordering::Acquire) == ATTACHED)
}

/// mpv has shown video on its own surface for a while: this path works on this TV.
pub fn video_worked() {
    if !VIDEO_WORKED.swap(true, Ordering::Relaxed) {
        tracing::info!("video shows on its own surface");
        call_activity_static("layeredVideoConfirmed");
    }
}

/// Video never appeared on mpv's surface: stop using it for this app version
/// and let mpv go. The caller continues with video drawn into the UI.
pub fn give_up() {
    tracing::error!("video never appeared on its own surface; drawing it into the UI from now on");
    ENABLED.store(false, Ordering::Relaxed);
    call_activity_static("layeringFailed");
    wake();
}

fn wake() {
    if let Some(worker) = WORKER.get() {
        let _ = worker.send(());
    }
}

/// The instance that should be attached now, and the attached one.
fn wanted_and_attached() -> (Option<Player>, Option<Player>) {
    let state = state();
    let wanted = if enabled() && state.want_video && state.surface.is_some() { state.current.clone() } else { None };
    (wanted, state.attached.clone())
}

fn already_right(wanted: &Option<Player>, attached: &Option<Player>) -> bool {
    match (attached, wanted) {
        (Some(a), Some(w)) => a.same_instance(w),
        (None, None) => true,
        _ => false,
    }
}

/// On the worker thread: attach the instance that should show video, if any,
/// and detach any other. [`MPV_CALLS`] is held for one mpv call at a time, so
/// `surfaceDestroyed` never waits behind more than one.
fn reconcile() {
    {
        let _calls = mpv_calls();
        let (wanted, attached) = wanted_and_attached();
        if already_right(&wanted, &attached) {
            return;
        }
        if attached.is_some() && !detach_attached() {
            // The old instance may still hold the surface; get a fresh one.
            // Attaching continues when the new surface arrives.
            tracing::warn!("video surface: previous player didn't let go; making a new surface");
            recreate_surface();
            return;
        }
    }
    let _calls = mpv_calls();
    let (wanted, attached) = wanted_and_attached();
    if attached.is_some() || already_right(&wanted, &attached) {
        // Changed meanwhile; the wake that changed it runs another pass.
        return;
    }
    let Some(player) = wanted else { return };
    let status = Arc::new(AtomicU8::new(ATTACHING));
    let (wid, width, height) = {
        let mut state = state();
        let Some(surface) = state.surface.as_ref() else { return };
        let target = (surface.object.as_obj().as_raw() as i64, surface.width, surface.height);
        // Recorded before the call: if it times out, mpv may still take the
        // surface, so it must be detached (or abandoned) like an attached one.
        state.attached = Some(player.clone());
        state.attach_status = Some(status.clone());
        target
    };
    if !VIDEO_WORKED.load(Ordering::Relaxed) {
        call_activity_static("layeredVideoStarting");
    }
    tracing::info!("video surface: attaching player ({width}x{height})");
    let call_status = status.clone();
    let outcome = with_timeout("attach", player, move |p| {
        let result = p.attach_android_surface(wid, width, height);
        call_status.store(if result.is_ok() { ATTACHED } else { ATTACH_FAILED }, Ordering::Release);
        result
    });
    match outcome {
        Outcome::Done => {}
        Outcome::Failed => {
            detach_attached();
        }
        // Stays recorded as attached; the next detach sorts it out.
        Outcome::TimedOut => {}
    }
}

/// Detach the attached instance (caller holds [`MPV_CALLS`]). Returns whether
/// mpv let go of the surface. If it didn't, or an attach call is still running
/// (it could set `wid` after the detach), the surface reference is abandoned,
/// never freed, since mpv may still use it.
fn detach_attached() -> bool {
    let (player, status) = {
        let mut state = state();
        (state.attached.take(), state.attach_status.take())
    };
    let Some(player) = player else { return true };
    let attach_running = status.is_some_and(|s| s.load(Ordering::Acquire) == ATTACHING);
    let detached = matches!(with_timeout("detach", player, |p| p.detach_android_surface()), Outcome::Done);
    let released = detached && !attach_running;
    if attach_running {
        tracing::warn!("video surface: detached while an attach was still running; keeping the surface reference");
    }
    if released {
        // mpv let go cleanly, so nothing crashed on this path.
        call_activity_static("layeredVideoWorked");
    } else {
        abandon(state().surface.take());
    }
    released
}

/// A stuck mpv instance may still use this reference as its `wid`: never free it.
fn abandon(surface: Option<Surface>) {
    if let Some(surface) = surface {
        std::mem::forget(surface.object);
    }
}

fn recreate_surface() {
    let Some(activity) = state().activity.clone() else { return };
    let Some(vm) = VM.get() else { return };
    let result = vm.attach_current_thread().map_err(|e| e.to_string()).and_then(|mut env| {
        env.call_method(activity.as_obj(), "recreateVideoSurface", "()V", &[])
            .map(|_| ())
            .map_err(|e| describe(&mut env, e))
    });
    if let Err(e) = result {
        tracing::error!("video surface: couldn't recreate it: {e}");
    }
}

enum Outcome {
    Done,
    Failed,
    TimedOut,
}

/// Run a blocking mpv call on a helper thread and wait up to [`MPV_TIMEOUT`].
/// A call that times out keeps running on its own thread.
fn with_timeout(what: &'static str, player: Player, call: impl FnOnce(&Player) -> joshflix_player::Result<()> + Send + 'static) -> Outcome {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name(format!("mpv-{what}")).spawn(move || {
        let _ = tx.send(call(&player));
    });
    if let Err(e) = spawned {
        tracing::error!("video surface: {what}: {e}");
        return Outcome::Failed;
    }
    match rx.recv_timeout(MPV_TIMEOUT) {
        Ok(Ok(())) => Outcome::Done,
        Ok(Err(e)) => {
            tracing::error!("video surface: {what}: {e}");
            Outcome::Failed
        }
        Err(_) => {
            tracing::error!("video surface: {what} took over {MPV_TIMEOUT:?}");
            Outcome::TimedOut
        }
    }
}

/// `MainActivity.nativeVideoSurface`, on Android's main thread. A null
/// `surface` means the video surface is going away: mpv must let go of it
/// before this returns.
extern "system" fn native_video_surface(env: JNIEnv, _class: JClass, surface: JObject, width: jint, height: jint) {
    if surface.is_null() {
        tracing::info!("video surface destroyed");
        let _calls = mpv_calls();
        if detach_attached() {
            state().surface = None;
        }
        return;
    }
    {
        let mut state = state();
        let same = state
            .surface
            .as_ref()
            .is_some_and(|s| env.is_same_object(s.object.as_obj(), &surface).unwrap_or(false));
        if same {
            if let Some(s) = state.surface.as_mut() {
                (s.width, s.height) = (width, height);
            }
            if let Some(player) = &state.attached {
                let _ = player.set_android_surface_size(width, height);
            }
            return;
        }
    }
    tracing::info!("video surface ready: {width}x{height}");
    {
        // A different surface without a destroy in between: let go of the old one.
        let _calls = mpv_calls();
        if detach_attached() {
            state().surface = None;
        }
    }
    match env.new_global_ref(&surface) {
        Ok(object) => state().surface = Some(Surface { object, width, height }),
        Err(e) => {
            tracing::error!("video surface: {e}");
            return;
        }
    }
    wake();
}
