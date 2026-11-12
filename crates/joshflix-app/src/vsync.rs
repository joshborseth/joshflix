//! Display vsync timestamps from Android's Choreographer.
//!
//! Animations look smooth when each frame advances them by exactly one
//! display refresh. Sampling the clock whenever the loop happens to wake
//! (after the previous `eglSwapBuffers` returned) adds that frame's render
//! time as jitter, which shows up as judder. The Choreographer reports when
//! each vsync happened, so the UI renders once per vsync and animates to
//! that vsync's timestamp.
//!
//! Callbacks run on a helper thread with its own looper, so they never
//! interrupt android-activity's polling of the main looper.

use std::cell::Cell;
use std::ffi::{c_long, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use android_activity::AndroidAppWaker;
use ndk::looper::{ForeignLooper, ThreadLooper};

type Callback64 = unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);
type CallbackLong = unsafe extern "C" fn(frame_time_nanos: c_long, data: *mut c_void);
type GetInstance = unsafe extern "C" fn() -> *mut c_void;
type Post64 = unsafe extern "C" fn(choreographer: *mut c_void, callback: Callback64, data: *mut c_void);
type PostLong = unsafe extern "C" fn(choreographer: *mut c_void, callback: CallbackLong, data: *mut c_void);

#[derive(Clone, Copy)]
enum Post {
    /// API 29+: 64-bit timestamps everywhere.
    V64(Post64),
    /// API 24-28: `long` timestamps, only 32 bits wide on 32-bit ARM.
    Long(PostLong),
}

/// `CLOCK_MONOTONIC` in nanoseconds, the clock Choreographer timestamps use.
// `time_t` and `c_long` are 32-bit on armv7 and 64-bit on arm64.
#[allow(clippy::useless_conversion)]
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    i64::from(ts.tv_sec) * 1_000_000_000 + i64::from(ts.tv_nsec)
}

#[derive(Default)]
struct Shared {
    /// The UI wants frames: keep a frame callback posted.
    wanted: AtomicBool,
    /// (sequence number, vsync time in `CLOCK_MONOTONIC` ns) of the latest vsync.
    latest: Mutex<(u64, i64)>,
    looper: OnceLock<ForeignLooper>,
}

/// State owned by the vsync thread and handed to each frame callback.
struct CallbackData {
    shared: Arc<Shared>,
    waker: AndroidAppWaker,
    posted: Cell<bool>,
}

impl CallbackData {
    fn record(&self, frame_time_ns: i64) {
        self.posted.set(false);
        if let Ok(mut latest) = self.shared.latest.lock() {
            *latest = (latest.0 + 1, frame_time_ns);
        }
        self.waker.wake();
    }
}

unsafe extern "C" fn on_frame_64(frame_time_nanos: i64, data: *mut c_void) {
    // SAFETY: `data` is the leaked CallbackData of this (the vsync) thread.
    let data = unsafe { &*data.cast::<CallbackData>() };
    data.record(frame_time_nanos);
}

#[allow(clippy::useless_conversion)] // `c_long` is 32-bit on armv7
unsafe extern "C" fn on_frame_long(frame_time_nanos: c_long, data: *mut c_void) {
    // SAFETY: as above.
    let data = unsafe { &*data.cast::<CallbackData>() };
    data.record(widen_timestamp(i64::from(frame_time_nanos), monotonic_ns()));
}

/// Rebuild a full timestamp from a `long` that may be truncated to 32 bits:
/// the latest time not after `now` whose low 32 bits match.
fn widen_timestamp(raw: i64, now: i64) -> i64 {
    if c_long::BITS >= 64 {
        return raw;
    }
    let low = raw & 0xFFFF_FFFF;
    let candidate = (now & !0xFFFF_FFFF) | low;
    if candidate > now { candidate - (1 << 32) } else { candidate }
}

/// A handle to the vsync thread.
pub struct Vsync {
    shared: Arc<Shared>,
}

impl Vsync {
    /// Start the vsync thread. `None` if this Android has no usable Choreographer.
    pub fn start(waker: AndroidAppWaker) -> Option<Self> {
        // SAFETY: libandroid is always present; the symbol types match the NDK headers.
        let (get_instance, post) = unsafe {
            let lib = libloading::Library::new("libandroid.so").ok()?;
            let get_instance = *lib.get::<GetInstance>(b"AChoreographer_getInstance\0").ok()?;
            let post = match lib.get::<Post64>(b"AChoreographer_postFrameCallback64\0") {
                Ok(f) => Post::V64(*f),
                Err(_) => Post::Long(*lib.get::<PostLong>(b"AChoreographer_postFrameCallback\0").ok()?),
            };
            // libandroid stays loaded for the life of the process anyway.
            std::mem::forget(lib);
            (get_instance, post)
        };

        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("vsync".into())
            .spawn(move || {
                crate::platform::raise_to_display_priority();
                let looper = ThreadLooper::prepare();
                // SAFETY: called on a thread with a looper, as required.
                let choreographer = unsafe { get_instance() };
                if choreographer.is_null() {
                    let _ = ready_tx.send(false);
                    return;
                }
                let _ = thread_shared.looper.set(looper.as_foreign().clone());
                let _ = ready_tx.send(true);

                // Lives as long as the thread, which lives as long as the process.
                let data: &'static CallbackData =
                    Box::leak(Box::new(CallbackData { shared: thread_shared, waker, posted: Cell::new(false) }));
                let data_ptr = std::ptr::from_ref(data).cast_mut().cast::<c_void>();
                loop {
                    if data.shared.wanted.load(Ordering::Acquire) && !data.posted.get() {
                        data.posted.set(true);
                        // SAFETY: valid choreographer for this thread; data outlives the callback.
                        unsafe {
                            match post {
                                Post::V64(f) => f(choreographer, on_frame_64, data_ptr),
                                Post::Long(f) => f(choreographer, on_frame_long, data_ptr),
                            }
                        }
                    }
                    // Runs frame callbacks; returns after each one or on wake().
                    if let Err(e) = looper.poll_once() {
                        tracing::warn!("vsync looper: {e}");
                    }
                }
            })
            .ok()?;
        ready_rx.recv().ok().filter(|ok| *ok)?;
        tracing::info!("frame pacing: Choreographer vsync");
        Some(Self { shared })
    }

    /// Ask for (or stop asking for) a callback on every vsync.
    pub fn set_wanted(&self, wanted: bool) {
        let was = self.shared.wanted.swap(wanted, Ordering::AcqRel);
        if wanted && !was && let Some(looper) = self.shared.looper.get() {
            looper.wake();
        }
    }

    /// The latest vsync: (sequence number, `CLOCK_MONOTONIC` ns).
    pub fn latest(&self) -> (u64, i64) {
        self.shared.latest.lock().map(|l| *l).unwrap_or_default()
    }
}

