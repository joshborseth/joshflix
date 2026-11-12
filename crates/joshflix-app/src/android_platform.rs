//! Our own Slint platform for Android TV: android-activity's event loop,
//! glutin EGL and Slint's femtovg OpenGL renderer.
//!
//! Slint's stock Android backend renders with Skia, which has no prebuilt
//! binaries for 32-bit ARM (still common on TVs) and doesn't tolerate other GL
//! code drawing into its context. femtovg does, which is how libmpv draws
//! video underneath the UI. Owning the loop also lets the remote's media keys
//! (play/pause, rewind, fast-forward) reach the app.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::vsync::{self, Vsync};

use android_activity::input::{InputEvent, KeyAction, KeyEvent, Keycode};
use android_activity::{AndroidApp, AndroidAppWaker, InputStatus, MainEvent, PollEvent};
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, PossiblyCurrentGlContext, Version};
use glutin::display::{Display, DisplayApiPreference, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use i_slint_renderer_femtovg::{FemtoVGOpenGLRendererExt, FemtoVGRendererExt};
use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};
use slint::platform::{EventLoopProxy, Key, Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{PhysicalSize, Window};

type FemtoVG = i_slint_renderer_femtovg::FemtoVGOpenGLRenderer;
type Job = Box<dyn FnOnce() + Send>;

thread_local! {
    /// Set by the UI's key callback so the loop knows whether a key was used.
    static KEY_HANDLED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Record whether the controller handled the key being dispatched.
pub fn report_key_handled(handled: bool) {
    KEY_HANDLED.with(|k| k.set(Some(handled)));
}

/// An EGL context + window surface for femtovg.
struct Egl {
    display: Display,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
}

// SAFETY: forwards to EGL; the context stays on the loop thread.
unsafe impl i_slint_renderer_femtovg::opengl::OpenGLInterface for Egl {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.context.is_current() {
            self.context.make_current(&self.surface)?;
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.surface.swap_buffers(&self.context)?;
        Ok(())
    }

    fn resize(&self, width: NonZeroU32, height: NonZeroU32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.surface.resize(&self.context, width, height);
        Ok(())
    }

    fn get_proc_address(&self, name: &CStr) -> *const c_void {
        self.display.get_proc_address(name)
    }
}

fn create_egl(window: &ndk::native_window::NativeWindow) -> Result<Egl, Box<dyn std::error::Error>> {
    let raw_display = RawDisplayHandle::Android(AndroidDisplayHandle::new());
    let ptr = NonNull::new(window.ptr().as_ptr().cast::<c_void>()).ok_or("null native window")?;
    let raw_window = RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(ptr));
    // SAFETY: the handles are valid for the lifetime of the native window.
    let display = unsafe { Display::new(raw_display, DisplayApiPreference::Egl)? };
    let template = ConfigTemplateBuilder::new()
        .with_alpha_size(8)
        .with_stencil_size(8)
        .compatible_with_native_window(raw_window)
        .build();
    // SAFETY: display is valid.
    let config = unsafe { display.find_configs(template)? }
        .reduce(|best, c| {
            // Prefer no MSAA (femtovg antialiases itself) and an 8-bit stencil.
            if c.num_samples() < best.num_samples() { c } else { best }
        })
        .ok_or("no EGL config")?;
    let attributes = |version| {
        ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(version)))
            .build(Some(raw_window))
    };
    // SAFETY: config and window come from the same display.
    let context = unsafe {
        display
            .create_context(&config, &attributes(Version::new(3, 0)))
            .or_else(|_| display.create_context(&config, &attributes(Version::new(2, 0))))?
    };
    let (width, height) = (window.width().max(1) as u32, window.height().max(1) as u32);
    let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        raw_window,
        NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
        NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
    );
    // SAFETY: as above.
    let surface = unsafe { display.create_window_surface(&config, &surface_attributes)? };
    let context = context.make_current(&surface)?;
    let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN));
    Ok(Egl { display, context, surface })
}

/// The single TV window.
pub struct TvWindow {
    window: Window,
    renderer: FemtoVG,
    size: Cell<PhysicalSize>,
    needs_redraw: Cell<bool>,
    has_surface: Cell<bool>,
}

impl WindowAdapter for TvWindow {
    fn window(&self) -> &Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        self.needs_redraw.set(true);
    }
}

struct Proxy {
    queue: Arc<Mutex<VecDeque<Job>>>,
    quit: Arc<Mutex<bool>>,
    waker: AndroidAppWaker,
}

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        if let Ok(mut quit) = self.quit.lock() {
            *quit = true;
        }
        self.waker.wake();
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Job) -> Result<(), slint::EventLoopError> {
        self.queue.lock().map_err(|_| slint::EventLoopError::EventLoopTerminated)?.push_back(event);
        self.waker.wake();
        Ok(())
    }
}

pub struct TvPlatform {
    app: AndroidApp,
    window: Rc<TvWindow>,
    queue: Arc<Mutex<VecDeque<Job>>>,
    quit: Arc<Mutex<bool>>,
    /// Keys the controller handles directly (media keys), bypassing Slint.
    direct_key: Box<dyn Fn(&str) -> bool>,
    /// Called when the activity pauses or resumes.
    lifecycle: Box<dyn Fn(bool)>,
    /// Display vsync, when the Choreographer is available.
    vsync: Option<Vsync>,
    /// `CLOCK_MONOTONIC` ns at startup: Slint's time zero.
    start_ns: i64,
    /// The vsync time animations step to this iteration (None: use the real clock).
    frame_ns: Cell<Option<i64>>,
    /// Last time handed to Slint, which requires a monotonic clock.
    clock_floor: Cell<Duration>,
}

/// A vsync older than this means frames weren't flowing; use the real clock.
const STALE_VSYNC_NS: i64 = 50_000_000;
/// Render anyway if the display stops reporting vsync while frames are wanted.
const VSYNC_TIMEOUT: Duration = Duration::from_millis(100);

impl TvPlatform {
    pub fn new(app: AndroidApp, direct_key: Box<dyn Fn(&str) -> bool>, lifecycle: Box<dyn Fn(bool)>) -> Result<Self, PlatformError> {
        let window = Rc::<TvWindow>::new_cyclic(|weak: &Weak<TvWindow>| TvWindow {
            window: Window::new(weak.clone() as Weak<dyn WindowAdapter>),
            renderer: <FemtoVG as FemtoVGRendererExt>::new_suspended(),
            size: Cell::new(PhysicalSize::new(1920, 1080)),
            needs_redraw: Cell::new(true),
            has_surface: Cell::new(false),
        });
        let vsync = Vsync::start(app.create_waker());
        if vsync.is_none() {
            tracing::info!("frame pacing: swap interval only (no Choreographer)");
        }
        Ok(Self {
            app,
            window,
            queue: Arc::default(),
            quit: Arc::default(),
            direct_key,
            lifecycle,
            vsync,
            start_ns: vsync::monotonic_ns(),
            frame_ns: Cell::new(None),
            clock_floor: Cell::new(Duration::ZERO),
        })
    }

    fn scale_factor(&self) -> f32 {
        // Android density in dpi; 160 dpi is scale 1. TVs report 320 (1080p) or 640 (4K).
        self.app.config().density().map_or(2.0, |dpi| dpi as f32 / 160.0).max(1.0)
    }

    fn init_surface(&self) {
        let Some(native) = self.app.native_window() else { return };
        match create_egl(&native) {
            Ok(egl) => {
                if let Err(e) = self.window.renderer.set_opengl_context(egl) {
                    tracing::error!("femtovg: {e}");
                    return;
                }
                self.window.has_surface.set(true);
                self.resize();
                crate::platform::android::request_ui_refresh_rate(&self.app);
            }
            Err(e) => tracing::error!("EGL setup failed: {e}"),
        }
    }

    fn destroy_surface(&self) {
        if self.window.has_surface.replace(false) {
            // Tells rendering notifiers (the video renderer) to free GPU state.
            let _ = self.window.renderer.clear_graphics_context();
        }
    }

    fn resize(&self) {
        let Some(native) = self.app.native_window() else { return };
        let size = PhysicalSize::new(native.width().max(1) as u32, native.height().max(1) as u32);
        let scale = self.scale_factor();
        self.window.size.set(size);
        self.window.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        self.window.window.dispatch_event(WindowEvent::Resized { size: size.to_logical(scale) });
        self.window.needs_redraw.set(true);
    }

    fn render(&self) {
        if !self.window.has_surface.get() {
            return;
        }
        self.window.needs_redraw.set(false);
        if let Err(e) = self.window.renderer.render() {
            tracing::warn!("render: {e}");
        }
    }

    fn handle_key(&self, key: &KeyEvent) -> InputStatus {
        let code = key.key_code();
        let down = matches!(key.action(), KeyAction::Down);
        let up = matches!(key.action(), KeyAction::Up);
        if !down && !up {
            return InputStatus::Unhandled;
        }
        // Media and TV keys go straight to the controller.
        let direct = match code {
            Keycode::MediaPlayPause => Some("playpause"),
            Keycode::MediaPlay => Some("play"),
            Keycode::MediaPause => Some("pause"),
            Keycode::MediaStop => Some("stop"),
            Keycode::MediaFastForward => Some("fastforward"),
            Keycode::MediaRewind => Some("rewind"),
            Keycode::MediaNext => Some("next"),
            Keycode::MediaPrevious => Some("previous"),
            Keycode::Captions => Some("captions"),
            Keycode::MediaAudioTrack => Some("audiotrack"),
            Keycode::Search => Some("search"),
            _ => None,
        };
        if let Some(name) = direct {
            if down && key.repeat_count() == 0 {
                (self.direct_key)(name);
            }
            self.window.needs_redraw.set(true);
            return InputStatus::Handled;
        }

        let text: Option<slint::SharedString> = match code {
            Keycode::DpadUp => Some(Key::UpArrow.into()),
            Keycode::DpadDown => Some(Key::DownArrow.into()),
            Keycode::DpadLeft => Some(Key::LeftArrow.into()),
            Keycode::DpadRight => Some(Key::RightArrow.into()),
            Keycode::DpadCenter | Keycode::Enter | Keycode::NumpadEnter | Keycode::ButtonA | Keycode::ButtonSelect => {
                Some(Key::Return.into())
            }
            Keycode::Back | Keycode::Escape | Keycode::ButtonB => Some(Key::Back.into()),
            Keycode::Del => Some(Key::Backspace.into()),
            Keycode::Menu | Keycode::Info | Keycode::Guide => Some(Key::Menu.into()),
            Keycode::ChannelUp | Keycode::PageUp => Some(Key::PageUp.into()),
            Keycode::ChannelDown | Keycode::PageDown => Some(Key::PageDown.into()),
            Keycode::Space => Some(" ".into()),
            other => {
                let raw = u32::from(other);
                // AKEYCODE_0..9 = 7..16, AKEYCODE_A..Z = 29..54
                match raw {
                    7..=16 => char::from_digit(raw - 7, 10).map(|c| c.to_string().into()),
                    29..=54 => char::from_u32(u32::from(b'a') + raw - 29).map(|c| c.to_string().into()),
                    _ => None,
                }
            }
        };
        // Volume, Home, Power and anything else stay with the system.
        let Some(text) = text else { return InputStatus::Unhandled };
        let is_back = code == Keycode::Back || code == Keycode::Escape || code == Keycode::ButtonB;

        KEY_HANDLED.with(|k| k.set(None));
        let event = if up {
            WindowEvent::KeyReleased { text }
        } else if key.repeat_count() > 0 {
            WindowEvent::KeyPressRepeated { text }
        } else {
            WindowEvent::KeyPressed { text }
        };
        self.window.window.dispatch_event(event);
        self.window.needs_redraw.set(true);
        let handled = KEY_HANDLED.with(Cell::get);

        // Holding Back must not walk out of the app: only a fresh press can finish it.
        if is_back && down && key.repeat_count() == 0 && handled == Some(false) {
            // Back on the top level: leave the app like any Android TV app.
            self.finish_activity();
        }
        if handled == Some(false) && !is_back { InputStatus::Unhandled } else { InputStatus::Handled }
    }

    fn finish_activity(&self) {
        // SAFETY: valid JavaVM/Activity pointers from android-activity.
        let Ok(vm) = (unsafe { jni::JavaVM::from_raw(self.app.vm_as_ptr().cast()) }) else { return };
        let Ok(mut env) = vm.attach_current_thread() else { return };
        // SAFETY: the activity global ref outlives the app.
        let activity = unsafe { jni::objects::JObject::from_raw(self.app.activity_as_ptr().cast()) };
        let _ = env.call_method(&activity, "finish", "()V", &[]);
    }

    fn drain_jobs(&self) {
        loop {
            let job = self.queue.lock().ok().and_then(|mut q| q.pop_front());
            match job {
                Some(job) => job(),
                None => break,
            }
        }
    }
}

/// Frames closer together than this belong to one animation.
const STATS_GAP_NS: i64 = 100_000_000;
/// How often the frame summary goes to the log while the UI animates.
const STATS_WINDOW_NS: i64 = 10_000_000_000;

/// Frame pacing summary for the TV log, to see whether animations drop frames.
#[derive(Default)]
struct FrameStats {
    start_ns: i64,
    frames: u32,
    /// Frames that followed another frame of the same animation.
    paced: u32,
    /// Vsyncs that passed without a new frame during animations.
    missed: u64,
    /// Frames that missed at least one vsync.
    late: u32,
    render_ns: Vec<i64>,
}

impl FrameStats {
    fn frame(&mut self, now_ns: i64, render_ns: i64, skipped: Option<u64>) {
        if self.frames == 0 {
            self.start_ns = now_ns;
        }
        self.frames += 1;
        self.render_ns.push(render_ns);
        if let Some(skipped) = skipped {
            self.paced += 1;
            self.missed += skipped;
            self.late += u32::from(skipped > 0);
        }
        if now_ns - self.start_ns >= STATS_WINDOW_NS {
            self.report();
            *self = Self::default();
        }
    }

    fn report(&mut self) {
        // Single redraws (a focus change with no animation) say nothing about pacing.
        if self.paced < 30 {
            return;
        }
        self.render_ns.sort_unstable();
        let ms = |ns: i64| ns as f64 / 1e6;
        let pick = |q: f64| self.render_ns[((self.render_ns.len() - 1) as f64 * q) as usize];
        tracing::info!(
            "frames: {} animated, {} late, {} vsyncs missed; render+swap median {:.1} ms, p95 {:.1} ms, max {:.1} ms",
            self.paced,
            self.late,
            self.missed,
            ms(pick(0.5)),
            ms(pick(0.95)),
            ms(pick(1.0)),
        );
    }
}

impl Platform for TvPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy {
            queue: self.queue.clone(),
            quit: self.quit.clone(),
            waker: self.app.create_waker(),
        }))
    }

    fn duration_since_start(&self) -> Duration {
        let ns = self.frame_ns.get().unwrap_or_else(vsync::monotonic_ns);
        let now = Duration::from_nanos(u64::try_from(ns - self.start_ns).unwrap_or(0));
        let now = now.max(self.clock_floor.get());
        self.clock_floor.set(now);
        now
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        crate::platform::raise_to_display_priority();
        let mut rendered_seq = 0u64;
        let mut last_render_ns = 0i64;
        let mut stats = FrameStats::default();
        loop {
            // While frames are flowing, animate to the latest vsync so every
            // frame advances by exactly one refresh, whenever the loop runs.
            let (seq, vsync_ns) = self.vsync.as_ref().map_or((0, 0), Vsync::latest);
            let now_ns = vsync::monotonic_ns();
            let fresh = self.vsync.is_some() && vsync_ns > 0 && now_ns - vsync_ns < STALE_VSYNC_NS;
            self.frame_ns.set(fresh.then_some(vsync_ns));
            slint::platform::update_timers_and_animations();
            self.frame_ns.set(None);

            // Nothing to draw on while backgrounded: init_surface() asks for a redraw on return.
            let wants_frame = self.window.has_surface.get()
                && (self.window.needs_redraw.get() || self.window.window.has_active_animations());
            if let Some(vsync) = &self.vsync {
                vsync.set_wanted(wants_frame);
            }
            // One frame per vsync. A fresh request after idle renders at once.
            let vsync_ready = self.vsync.is_none()
                || seq != rendered_seq
                || now_ns - last_render_ns > VSYNC_TIMEOUT.as_nanos() as i64;
            if wants_frame && vsync_ready {
                // Vsyncs that passed since the previous frame of an ongoing animation.
                let skipped = (now_ns - last_render_ns < STATS_GAP_NS && self.vsync.is_some())
                    .then(|| seq.saturating_sub(rendered_seq).saturating_sub(1));
                rendered_seq = seq;
                last_render_ns = now_ns;
                self.render();
                stats.frame(now_ns, vsync::monotonic_ns() - now_ns, skipped);
            }

            let pending = self.window.has_surface.get()
                && (self.window.needs_redraw.get() || self.window.window.has_active_animations());
            let timeout = if !pending {
                slint::platform::duration_until_next_timer_update().or(Some(Duration::from_millis(500)))
            } else if self.vsync.is_some() {
                // The vsync thread wakes the looper; the timeout is only a fallback.
                Some(VSYNC_TIMEOUT)
            } else {
                Some(Duration::ZERO)
            };

            let mut destroy = false;
            self.app.poll_events(timeout, |event| {
                if let PollEvent::Main(main) = event {
                    match main {
                        MainEvent::InitWindow { .. } => self.init_surface(),
                        MainEvent::TerminateWindow { .. } => self.destroy_surface(),
                        MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } | MainEvent::ConfigChanged { .. } => {
                            self.resize()
                        }
                        MainEvent::RedrawNeeded { .. } | MainEvent::GainedFocus => self.window.needs_redraw.set(true),
                        MainEvent::Pause => (self.lifecycle)(false),
                        MainEvent::Resume { .. } => (self.lifecycle)(true),
                        MainEvent::Destroy => destroy = true,
                        _ => {}
                    }
                }
            });

            if let Ok(mut events) = self.app.input_events_iter() {
                while events.next(|event| match event {
                    InputEvent::KeyEvent(key) => self.handle_key(key),
                    _ => InputStatus::Unhandled,
                }) {}
            }

            self.drain_jobs();
            if destroy || self.quit.lock().map(|q| *q).unwrap_or(true) {
                if let Some(vsync) = &self.vsync {
                    vsync.set_wanted(false);
                }
                self.destroy_surface();
                return Ok(());
            }
        }
    }
}
