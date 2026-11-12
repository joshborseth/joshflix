//! Draws libmpv video underneath the Slint UI in the same OpenGL context.
//!
//! Slint clears the frame, calls our `BeforeRendering` hook (mpv renders the
//! current video frame into the default framebuffer), then draws the UI on
//! top. mpv's "new frame" callback asks Slint for a redraw.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::rc::Rc;

use joshflix_player::{Player, TrackKind, VideoRenderer};
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

/// Saves and restores every piece of GL state mpv's renderer may touch, so the
/// UI renderer's own state cache stays valid. Slint's Android backend renders
/// with Skia, which caches GL state and never re-queries it; femtovg (desktop)
/// is more forgiving but gets the same treatment.
struct GlState {
    f: GlFns,
    max_units: u32,
}

#[allow(clippy::type_complexity)]
struct GlFns {
    get_integerv: unsafe extern "C" fn(u32, *mut i32),
    get_booleanv: unsafe extern "C" fn(u32, *mut u8),
    is_enabled: unsafe extern "C" fn(u32) -> u8,
    enable: unsafe extern "C" fn(u32),
    disable: unsafe extern "C" fn(u32),
    get_error: unsafe extern "C" fn() -> u32,
    active_texture: unsafe extern "C" fn(u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    bind_buffer: unsafe extern "C" fn(u32, u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    bind_renderbuffer: unsafe extern "C" fn(u32, u32),
    use_program: unsafe extern "C" fn(u32),
    viewport: unsafe extern "C" fn(i32, i32, i32, i32),
    scissor: unsafe extern "C" fn(i32, i32, i32, i32),
    blend_func_separate: unsafe extern "C" fn(u32, u32, u32, u32),
    blend_equation_separate: unsafe extern "C" fn(u32, u32),
    color_mask: unsafe extern "C" fn(u8, u8, u8, u8),
    stencil_func_separate: unsafe extern "C" fn(u32, u32, i32, u32),
    stencil_op_separate: unsafe extern "C" fn(u32, u32, u32, u32),
    stencil_mask_separate: unsafe extern "C" fn(u32, u32),
    pixel_storei: unsafe extern "C" fn(u32, i32),
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    get_floatv: unsafe extern "C" fn(u32, *mut f32),
    // Optional on GLES2.
    bind_vertex_array: Option<unsafe extern "C" fn(u32)>,
    bind_sampler: Option<unsafe extern "C" fn(u32, u32)>,
}

/// A snapshot of the state we restore.
#[derive(Default)]
struct Snapshot {
    program: i32,
    vao: i32,
    array_buffer: i32,
    unpack_buffer: i32,
    pack_buffer: i32,
    draw_fbo: i32,
    read_fbo: i32,
    renderbuffer: i32,
    active_texture: i32,
    textures_2d: Vec<i32>,
    textures_ext: Vec<i32>,
    samplers: Vec<i32>,
    viewport: [i32; 4],
    scissor_box: [i32; 4],
    enabled: Vec<(u32, bool)>,
    blend: [i32; 6], // src rgb, dst rgb, src a, dst a, eq rgb, eq a
    color_mask: [u8; 4],
    stencil: [[i32; 7]; 2], // func, ref, value mask, fail, zfail, zpass, writemask (front/back)
    pixel_store: Vec<(u32, i32)>,
    clear_color: [f32; 4],
}

mod gl {
    pub const CURRENT_PROGRAM: u32 = 0x8B8D;
    pub const VERTEX_ARRAY_BINDING: u32 = 0x85B5;
    pub const ARRAY_BUFFER: u32 = 0x8892;
    pub const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    pub const PIXEL_PACK_BUFFER: u32 = 0x88EB;
    pub const PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
    pub const PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;
    pub const PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x88EF;
    pub const DRAW_FRAMEBUFFER: u32 = 0x8CA9;
    pub const READ_FRAMEBUFFER: u32 = 0x8CA8;
    pub const DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    pub const READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
    pub const FRAMEBUFFER: u32 = 0x8D40;
    pub const RENDERBUFFER: u32 = 0x8D41;
    pub const RENDERBUFFER_BINDING: u32 = 0x8CA7;
    pub const ACTIVE_TEXTURE: u32 = 0x84E0;
    pub const TEXTURE0: u32 = 0x84C0;
    pub const TEXTURE_2D: u32 = 0x0DE1;
    pub const TEXTURE_BINDING_2D: u32 = 0x8069;
    pub const TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
    pub const TEXTURE_BINDING_EXTERNAL_OES: u32 = 0x8D67;
    pub const SAMPLER_BINDING: u32 = 0x8919;
    pub const MAX_COMBINED_TEXTURE_IMAGE_UNITS: u32 = 0x8B4D;
    pub const VIEWPORT: u32 = 0x0BA2;
    pub const SCISSOR_BOX: u32 = 0x0C10;
    pub const SCISSOR_TEST: u32 = 0x0C11;
    pub const BLEND: u32 = 0x0BE2;
    pub const DEPTH_TEST: u32 = 0x0B71;
    pub const STENCIL_TEST: u32 = 0x0B90;
    pub const CULL_FACE: u32 = 0x0B44;
    pub const DITHER: u32 = 0x0BD0;
    pub const BLEND_SRC_RGB: u32 = 0x80C9;
    pub const BLEND_DST_RGB: u32 = 0x80C8;
    pub const BLEND_SRC_ALPHA: u32 = 0x80CB;
    pub const BLEND_DST_ALPHA: u32 = 0x80CA;
    pub const BLEND_EQUATION_RGB: u32 = 0x8009;
    pub const BLEND_EQUATION_ALPHA: u32 = 0x883D;
    pub const COLOR_WRITEMASK: u32 = 0x0C23;
    pub const COLOR_CLEAR_VALUE: u32 = 0x0C22;
    pub const FRONT: u32 = 0x0404;
    pub const BACK: u32 = 0x0405;
    pub const STENCIL_FUNC: u32 = 0x0B92;
    pub const STENCIL_REF: u32 = 0x0B97;
    pub const STENCIL_VALUE_MASK: u32 = 0x0B93;
    pub const STENCIL_FAIL: u32 = 0x0B94;
    pub const STENCIL_PASS_DEPTH_FAIL: u32 = 0x0B95;
    pub const STENCIL_PASS_DEPTH_PASS: u32 = 0x0B96;
    pub const STENCIL_WRITEMASK: u32 = 0x0B98;
    pub const STENCIL_BACK_FUNC: u32 = 0x8800;
    pub const STENCIL_BACK_REF: u32 = 0x8CA3;
    pub const STENCIL_BACK_VALUE_MASK: u32 = 0x8CA4;
    pub const STENCIL_BACK_FAIL: u32 = 0x8801;
    pub const STENCIL_BACK_PASS_DEPTH_FAIL: u32 = 0x8802;
    pub const STENCIL_BACK_PASS_DEPTH_PASS: u32 = 0x8803;
    pub const STENCIL_BACK_WRITEMASK: u32 = 0x8CA5;
    pub const UNPACK_ALIGNMENT: u32 = 0x0CF5;
    pub const UNPACK_ROW_LENGTH: u32 = 0x0CF2;
    pub const UNPACK_SKIP_ROWS: u32 = 0x0CF3;
    pub const UNPACK_SKIP_PIXELS: u32 = 0x0CF4;
    pub const PACK_ALIGNMENT: u32 = 0x0D05;
    pub const PACK_ROW_LENGTH: u32 = 0x0D02;
}

impl GlState {
    fn load(get: &dyn Fn(&CStr) -> *const c_void) -> Option<Self> {
        macro_rules! f {
            ($name:literal) => {{
                let ptr = get(CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).ok()?);
                if ptr.is_null() {
                    return None;
                }
                // SAFETY: the symbol is the GL entry point with the declared signature.
                unsafe { std::mem::transmute::<*const c_void, _>(ptr) }
            }};
        }
        macro_rules! opt {
            ($name:literal) => {{
                let ptr = get(CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).ok()?);
                // SAFETY: as above.
                (!ptr.is_null()).then(|| unsafe { std::mem::transmute::<*const c_void, _>(ptr) })
            }};
        }
        let f = GlFns {
            get_integerv: f!("glGetIntegerv"),
            get_booleanv: f!("glGetBooleanv"),
            is_enabled: f!("glIsEnabled"),
            enable: f!("glEnable"),
            disable: f!("glDisable"),
            get_error: f!("glGetError"),
            active_texture: f!("glActiveTexture"),
            bind_texture: f!("glBindTexture"),
            bind_buffer: f!("glBindBuffer"),
            bind_framebuffer: f!("glBindFramebuffer"),
            bind_renderbuffer: f!("glBindRenderbuffer"),
            use_program: f!("glUseProgram"),
            viewport: f!("glViewport"),
            scissor: f!("glScissor"),
            blend_func_separate: f!("glBlendFuncSeparate"),
            blend_equation_separate: f!("glBlendEquationSeparate"),
            color_mask: f!("glColorMask"),
            stencil_func_separate: f!("glStencilFuncSeparate"),
            stencil_op_separate: f!("glStencilOpSeparate"),
            stencil_mask_separate: f!("glStencilMaskSeparate"),
            pixel_storei: f!("glPixelStorei"),
            clear_color: f!("glClearColor"),
            get_floatv: f!("glGetFloatv"),
            bind_vertex_array: opt!("glBindVertexArray"),
            bind_sampler: opt!("glBindSampler"),
        };
        let mut units = 0;
        // SAFETY: GL context is current (called from the rendering notifier).
        unsafe { (f.get_integerv)(gl::MAX_COMBINED_TEXTURE_IMAGE_UNITS, &mut units) };
        let state = Self { max_units: (units.max(1) as u32).min(8), f };
        state.drain_errors();
        Some(state)
    }

    fn int(&self, name: u32) -> i32 {
        let mut v = 0;
        // SAFETY: GL context is current; unknown enums leave `v` untouched.
        unsafe { (self.f.get_integerv)(name, &mut v) };
        v
    }

    fn drain_errors(&self) {
        for _ in 0..16 {
            // SAFETY: GL context is current.
            if unsafe { (self.f.get_error)() } == 0 {
                break;
            }
        }
    }

    fn capture(&self) -> Snapshot {
        let mut s = Snapshot {
            program: self.int(gl::CURRENT_PROGRAM),
            vao: if self.f.bind_vertex_array.is_some() { self.int(gl::VERTEX_ARRAY_BINDING) } else { 0 },
            array_buffer: self.int(gl::ARRAY_BUFFER_BINDING),
            unpack_buffer: self.int(gl::PIXEL_UNPACK_BUFFER_BINDING),
            pack_buffer: self.int(gl::PIXEL_PACK_BUFFER_BINDING),
            draw_fbo: self.int(gl::DRAW_FRAMEBUFFER_BINDING),
            read_fbo: self.int(gl::READ_FRAMEBUFFER_BINDING),
            renderbuffer: self.int(gl::RENDERBUFFER_BINDING),
            active_texture: self.int(gl::ACTIVE_TEXTURE),
            ..Snapshot::default()
        };
        // SAFETY: GL context is current; all out-pointers are valid.
        unsafe {
            for unit in 0..self.max_units {
                (self.f.active_texture)(gl::TEXTURE0 + unit);
                s.textures_2d.push(self.int(gl::TEXTURE_BINDING_2D));
                s.textures_ext.push(self.int(gl::TEXTURE_BINDING_EXTERNAL_OES));
                if self.f.bind_sampler.is_some() {
                    s.samplers.push(self.int(gl::SAMPLER_BINDING));
                }
            }
            (self.f.active_texture)(s.active_texture as u32);
            (self.f.get_integerv)(gl::VIEWPORT, s.viewport.as_mut_ptr());
            (self.f.get_integerv)(gl::SCISSOR_BOX, s.scissor_box.as_mut_ptr());
            (self.f.get_booleanv)(gl::COLOR_WRITEMASK, s.color_mask.as_mut_ptr());
            (self.f.get_floatv)(gl::COLOR_CLEAR_VALUE, s.clear_color.as_mut_ptr());
            for cap in [gl::SCISSOR_TEST, gl::BLEND, gl::DEPTH_TEST, gl::STENCIL_TEST, gl::CULL_FACE, gl::DITHER] {
                s.enabled.push((cap, (self.f.is_enabled)(cap) != 0));
            }
        }
        s.blend = [
            self.int(gl::BLEND_SRC_RGB),
            self.int(gl::BLEND_DST_RGB),
            self.int(gl::BLEND_SRC_ALPHA),
            self.int(gl::BLEND_DST_ALPHA),
            self.int(gl::BLEND_EQUATION_RGB),
            self.int(gl::BLEND_EQUATION_ALPHA),
        ];
        s.stencil = [
            [gl::STENCIL_FUNC, gl::STENCIL_REF, gl::STENCIL_VALUE_MASK, gl::STENCIL_FAIL, gl::STENCIL_PASS_DEPTH_FAIL, gl::STENCIL_PASS_DEPTH_PASS, gl::STENCIL_WRITEMASK]
                .map(|e| self.int(e)),
            [gl::STENCIL_BACK_FUNC, gl::STENCIL_BACK_REF, gl::STENCIL_BACK_VALUE_MASK, gl::STENCIL_BACK_FAIL, gl::STENCIL_BACK_PASS_DEPTH_FAIL, gl::STENCIL_BACK_PASS_DEPTH_PASS, gl::STENCIL_BACK_WRITEMASK]
                .map(|e| self.int(e)),
        ];
        for e in [gl::UNPACK_ALIGNMENT, gl::UNPACK_ROW_LENGTH, gl::UNPACK_SKIP_ROWS, gl::UNPACK_SKIP_PIXELS, gl::PACK_ALIGNMENT, gl::PACK_ROW_LENGTH] {
            s.pixel_store.push((e, self.int(e)));
        }
        self.drain_errors();
        s
    }

    fn restore(&self, s: &Snapshot) {
        // SAFETY: GL context is current; values came from capture() on the same context.
        unsafe {
            (self.f.use_program)(s.program as u32);
            if let Some(bind) = self.f.bind_vertex_array {
                bind(s.vao as u32);
            }
            (self.f.bind_buffer)(gl::ARRAY_BUFFER, s.array_buffer as u32);
            (self.f.bind_buffer)(gl::PIXEL_UNPACK_BUFFER, s.unpack_buffer as u32);
            (self.f.bind_buffer)(gl::PIXEL_PACK_BUFFER, s.pack_buffer as u32);
            if s.draw_fbo == s.read_fbo {
                (self.f.bind_framebuffer)(gl::FRAMEBUFFER, s.draw_fbo as u32);
            } else {
                (self.f.bind_framebuffer)(gl::DRAW_FRAMEBUFFER, s.draw_fbo as u32);
                (self.f.bind_framebuffer)(gl::READ_FRAMEBUFFER, s.read_fbo as u32);
            }
            (self.f.bind_renderbuffer)(gl::RENDERBUFFER, s.renderbuffer as u32);
            for unit in 0..self.max_units as usize {
                (self.f.active_texture)(gl::TEXTURE0 + unit as u32);
                (self.f.bind_texture)(gl::TEXTURE_2D, s.textures_2d[unit] as u32);
                (self.f.bind_texture)(gl::TEXTURE_EXTERNAL_OES, s.textures_ext[unit] as u32);
                if let (Some(bind), Some(sampler)) = (self.f.bind_sampler, s.samplers.get(unit)) {
                    bind(unit as u32, *sampler as u32);
                }
            }
            (self.f.active_texture)(s.active_texture as u32);
            (self.f.viewport)(s.viewport[0], s.viewport[1], s.viewport[2], s.viewport[3]);
            (self.f.scissor)(s.scissor_box[0], s.scissor_box[1], s.scissor_box[2], s.scissor_box[3]);
            for (cap, on) in &s.enabled {
                if *on { (self.f.enable)(*cap) } else { (self.f.disable)(*cap) }
            }
            (self.f.blend_func_separate)(s.blend[0] as u32, s.blend[1] as u32, s.blend[2] as u32, s.blend[3] as u32);
            (self.f.blend_equation_separate)(s.blend[4] as u32, s.blend[5] as u32);
            (self.f.color_mask)(s.color_mask[0], s.color_mask[1], s.color_mask[2], s.color_mask[3]);
            (self.f.clear_color)(s.clear_color[0], s.clear_color[1], s.clear_color[2], s.clear_color[3]);
            for (face, st) in [(gl::FRONT, s.stencil[0]), (gl::BACK, s.stencil[1])] {
                (self.f.stencil_func_separate)(face, st[0] as u32, st[1], st[2] as u32);
                (self.f.stencil_op_separate)(face, st[3] as u32, st[4] as u32, st[5] as u32);
                (self.f.stencil_mask_separate)(face, st[6] as u32);
            }
            for (e, v) in &s.pixel_store {
                (self.f.pixel_storei)(*e, *v);
            }
        }
        self.drain_errors();
    }
}

#[derive(Default)]
pub struct VideoHost {
    player: RefCell<Option<Player>>,
    renderer: RefCell<Option<VideoRenderer>>,
    gl: RefCell<Option<GlState>>,
    visible: Cell<bool>,
    unavailable: RefCell<Option<String>>,
    /// Instance id of the current player; events from abandoned instances are dropped.
    current_id: Cell<u64>,
    /// The current player decodes in compatibility (copy-back) mode.
    compat: Cell<bool>,
    /// Video was turned off because the GL surface went away (Android
    /// background, or a TV that rebuilds its window on a display-mode switch).
    /// Holds the video track that was playing, if the track list knew it.
    video_suspended: Cell<Option<Option<i64>>>,
    /// Times the UI's drawing on the GPU for the TV log's frame summary.
    gpu_timer: RefCell<Option<crate::gpu_timer::GpuTimer>>,
    /// Timer queries were set up (or found unsupported) for this GL context.
    gpu_timer_tried: Cell<bool>,
}

static NEXT_PLAYER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl VideoHost {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn set_player(&self, player: Result<(Player, u64), String>, compat: bool) {
        match player {
            Ok((player, id)) => {
                *self.player.borrow_mut() = Some(player);
                self.current_id.set(id);
                self.compat.set(compat);
            }
            Err(reason) => {
                tracing::error!("player unavailable: {reason}");
                *self.unavailable.borrow_mut() = Some(reason);
            }
        }
    }

    /// Create an mpv instance whose events go to the controller, tagged with an id.
    /// `audio_session` is the platform's audio session for mpv's AudioTrack output.
    pub fn create_player(compat_mode: bool, audio_session: Option<i32>) -> Result<(Player, u64), String> {
        let mut config = joshflix_player::PlayerConfig::for_platform();
        config.audio_session = audio_session;
        if compat_mode && cfg!(target_os = "android") && std::env::var("JOSHFLIX_HWDEC").is_err() {
            config.hwdec = "mediacodec-copy,no".into();
        }
        let id = NEXT_PLAYER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("player {id}: hwdec={} compat={compat_mode}", config.hwdec);
        Player::new(&config, move |event| {
            // Log mpv's messages right here on its event thread, so they still
            // reach logcat and the relay when the UI thread is stuck.
            if let joshflix_player::PlayerEvent::Log { level, text } = &event {
                match level.as_str() {
                    "error" | "fatal" => tracing::error!("mpv[{id}]: {text}"),
                    "warn" => tracing::warn!("mpv[{id}]: {text}"),
                    _ => tracing::info!("mpv[{id}][{level}]: {text}"),
                }
                // Only errors matter to the UI (shown with a failed playback).
                if !matches!(level.as_str(), "error" | "fatal") {
                    return;
                }
            }
            let _ = slint::invoke_from_event_loop(move || crate::app::with_controller(|c| c.on_player_event(id, event)));
        })
        .map(|player| (player, id))
        .map_err(|e| e.to_string())
    }

    pub fn current_id(&self) -> u64 {
        self.current_id.get()
    }

    pub fn is_compat(&self) -> bool {
        self.compat.get()
    }

    /// Swap in a fresh mpv instance. The old one is told to stop (queued, never
    /// blocks) and then abandoned without cleanup: this is only used when its
    /// decoder is wedged, and freeing it would block.
    pub fn replace_player(&self, (player, id): (Player, u64), compat: bool) {
        if let Some(renderer) = self.renderer.borrow_mut().take() {
            std::mem::forget(renderer);
        }
        if let Some(old) = self.player.borrow_mut().replace(player) {
            let _ = old.stop();
            std::mem::forget(old);
        }
        self.current_id.set(id);
        self.compat.set(compat);
        self.video_suspended.set(None);
    }

    pub fn player(&self) -> Option<Player> {
        self.player.borrow().clone()
    }

    pub fn unavailable_reason(&self) -> Option<String> {
        self.unavailable.borrow().clone()
    }

    pub fn set_visible(&self, visible: bool) {
        self.visible.set(visible);
    }

    /// Hook into the window's rendering.
    pub fn attach(self: &Rc<Self>, ui: &AppWindow) {
        let host = self.clone();
        let weak = ui.as_weak();
        let result = ui.window().set_rendering_notifier(move |state, api| {
            let GraphicsAPI::NativeOpenGL { get_proc_address } = api else { return };
            match state {
                RenderingState::RenderingSetup => {}
                RenderingState::BeforeRendering => {
                    let Some(ui) = weak.upgrade() else { return };
                    let size = ui.window().size();
                    if host.renderer.borrow().is_none()
                        && let Some(player) = host.player()
                    {
                        let redraw = weak.clone();
                        let on_frame: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                            let redraw = redraw.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = redraw.upgrade() {
                                    ui.window().request_redraw();
                                }
                            });
                        });
                        // SAFETY: Slint's GL context is current during BeforeRendering.
                        match unsafe { VideoRenderer::new(&player, get_proc_address, on_frame) } {
                            Ok(renderer) => {
                                *host.renderer.borrow_mut() = Some(renderer);
                                *host.gl.borrow_mut() = GlState::load(get_proc_address);
                                // Freeing the old render context disabled video; turn it back on.
                                if let Some(track) = host.video_suspended.take() {
                                    restore_video(&player, track);
                                }
                            }
                            Err(e) => tracing::error!("video renderer: {e}"),
                        }
                    }
                    // mpv needs the render context before it can open any file, so it's
                    // created on the first frame; video is only drawn while visible.
                    if host.visible.get()
                        && let Some(renderer) = host.renderer.borrow().as_ref()
                    {
                        let started = crate::perf::now_ns();
                        let gl = host.gl.borrow();
                        let snapshot = gl.as_ref().map(GlState::capture);
                        // Render into whatever framebuffer the UI renderer is drawing to.
                        let fbo = snapshot.as_ref().map_or(0, |s| s.draw_fbo);
                        renderer.render(fbo, size.width as i32, size.height as i32);
                        if let (Some(gl), Some(snapshot)) = (gl.as_ref(), snapshot.as_ref()) {
                            gl.restore(snapshot);
                        }
                        crate::perf::video_drawn(crate::perf::now_ns() - started);
                    }
                    // Timed after the video: mpv runs its own timer queries, and two
                    // can't be active at once.
                    if !host.gpu_timer_tried.replace(true) {
                        // SAFETY: Slint's GL context is current during BeforeRendering.
                        *host.gpu_timer.borrow_mut() = unsafe { crate::gpu_timer::GpuTimer::new(get_proc_address) };
                    }
                    if let Some(timer) = host.gpu_timer.borrow_mut().as_mut() {
                        timer.begin();
                    }
                    crate::perf::draw_started(crate::perf::now_ns());
                }
                RenderingState::AfterRendering => {
                    crate::perf::draw_finished(crate::perf::now_ns());
                    if let Some(timer) = host.gpu_timer.borrow_mut().as_mut() {
                        timer.end();
                    }
                    if host.visible.get()
                        && let Some(renderer) = host.renderer.borrow().as_ref()
                    {
                        renderer.report_swap();
                    }
                }
                RenderingState::RenderingTeardown => {
                    // The GL context is going away (e.g. Android pause): free GPU state now.
                    // mpv disables video when its render context is freed mid-file, so
                    // remember the track to re-enable when a new context arrives. Read
                    // the cached track list: it still shows the selection before mpv
                    // drops the video, and reading it never blocks on mpv.
                    if host.renderer.borrow().is_some()
                        && let Some(player) = host.player()
                    {
                        let video = player.tracks().into_iter().find(|t| t.kind == TrackKind::Video && t.selected);
                        if let Some(track) = video {
                            host.video_suspended.set(Some(Some(track.id)));
                        } else if host.visible.get() {
                            host.video_suspended.set(Some(None));
                        }
                    }
                    host.renderer.borrow_mut().take();
                    host.gl.borrow_mut().take();
                    host.gpu_timer.borrow_mut().take();
                    host.gpu_timer_tried.set(false);
                }
                _ => {}
            }
        });
        if let Err(e) = result {
            tracing::error!("rendering notifier unavailable: {e:?}");
        }
    }
}

/// Turn video back on after mpv dropped it because its render context was freed.
///
/// mpv deselects the track and resets `vid` to `auto` when that happens, so
/// setting `vid=auto` again changes nothing and leaves the picture black with
/// sound playing. Selecting the track by id always switches. Without a known
/// id, go through `no` so that `auto` is a real change and mpv picks again.
fn restore_video(player: &Player, track: Option<i64>) {
    tracing::info!("GL surface is back: re-enabling video (track {track:?})");
    let _ = match track {
        Some(id) => player.select_track(TrackKind::Video, Some(id)),
        None => player.set_property("vid", "no").and_then(|()| player.set_property("vid", "auto")),
    };
}
