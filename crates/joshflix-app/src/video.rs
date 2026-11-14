//! The video players, and drawing mpv's video into the UI.
//!
//! [`VideoHost`] owns the players and says which one plays ([`Engine`]):
//! - ExoPlayer on Android TVs ([`crate::exo`]), on its own video layer;
//! - mpv everywhere else, and on a TV for files ExoPlayer can't play. On
//!   Android it draws on its own layer too ([`crate::video_surface`]); on
//!   desktop, or a TV without layers, it draws into the UI's OpenGL context:
//!   Slint clears the frame, calls our `BeforeRendering` hook (mpv renders
//!   the current video frame into the default framebuffer), then draws the UI
//!   on top. mpv's "new frame" callback asks Slint for a redraw.
//!
//! [`Playback`] is the player the controller talks to, whichever it is.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::rc::Rc;

use joshflix_player::{Player, PlayerEvent, SeparateTracks, Track, TrackKind, VideoRenderer};
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

/// Which player plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Mpv,
    /// Media3 ExoPlayer (Android).
    Exo,
}

/// The player that plays now, mpv or ExoPlayer. Cloning shares it. Every
/// call returns at once.
#[derive(Clone)]
pub enum Playback {
    Mpv(Player),
    #[cfg(target_os = "android")]
    Exo(crate::exo::Exo),
}

impl Playback {
    pub fn engine(&self) -> Engine {
        match self {
            Playback::Mpv(_) => Engine::Mpv,
            #[cfg(target_os = "android")]
            Playback::Exo(_) => Engine::Exo,
        }
    }

    pub fn is_exo(&self) -> bool {
        self.engine() == Engine::Exo
    }

    /// The mpv instance, when mpv plays.
    pub fn mpv(&self) -> Option<&Player> {
        match self {
            Playback::Mpv(player) => Some(player),
            #[cfg(target_os = "android")]
            Playback::Exo(_) => None,
        }
    }

    pub fn load(&self, url: &str, start_secs: Option<f64>) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.load(url, start_secs),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.load(url, start_secs),
        }
    }

    pub fn stop(&self) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.stop(),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.stop(),
        }
    }

    pub fn set_paused(&self, paused: bool) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.set_paused(paused),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.set_paused(paused),
        }
    }

    pub fn toggle_pause(&self) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.toggle_pause(),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.toggle_pause(),
        }
    }

    pub fn seek_to(&self, secs: f64) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.seek_to(secs),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.seek_to(secs),
        }
    }

    pub fn seek_by(&self, secs: f64) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.seek_by(secs),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.seek_by(secs),
        }
    }

    /// `None` disables the track type (subtitles off).
    pub fn select_track(&self, kind: TrackKind, id: Option<i64>) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.select_track(kind, id),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.select_track(kind, id),
        }
    }

    /// Add an external subtitle without selecting it.
    pub fn add_subtitle(&self, url: &str, title: &str, lang: &str) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.add_subtitle(url, title, lang),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.add_subtitle(url, title, lang),
        }
    }

    pub fn set_subtitle_scale(&self, scale: f64) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.set_subtitle_scale(scale),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.set_subtitle_scale(scale),
        }
    }

    /// Lift subtitles above the player's controls while they show.
    pub fn set_subtitles_lifted(&self, lifted: bool) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.set_property("sub-margin-y", if lifted { "150" } else { "48" }),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.set_subtitles_lifted(lifted),
        }
    }

    /// Hide subtitles for a while (ExoPlayer's draw above the UI). mpv's
    /// draw under the UI, which covers them by itself.
    pub fn set_subtitles_hidden(&self, hidden: bool) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(_) => {
                let _ = hidden;
                Ok(())
            }
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.set_subtitles_hidden(hidden),
        }
    }

    /// The audio language to prefer (the one picked last time), from the next load on.
    pub fn set_audio_language(&self, language: Option<&str>) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.set_property("alang", language.unwrap_or("")),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => {
                e.set_audio_language(language);
                Ok(())
            }
        }
    }

    /// The player's own Audio Normalization filter (mpv's lavfi chain).
    pub fn set_audio_normalization(&self, enabled: bool) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.set_audio_normalization(enabled),
            #[cfg(target_os = "android")]
            Playback::Exo(_) => Ok(()),
        }
    }

    /// A platform effect normalizes audio (`on`): it only sees decoded audio,
    /// so ExoPlayer stops passing Dolby and DTS through meanwhile.
    pub fn set_platform_normalization(&self, on: bool) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(_) => {
                let _ = on;
                Ok(())
            }
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.set_pcm_only(on),
        }
    }

    /// mpv: read these tracks over their own connections (see [`SeparateTracks`]).
    pub fn separate_tracks(&self, url: &str, tracks: SeparateTracks) -> joshflix_player::Result<()> {
        match self {
            Playback::Mpv(p) => p.separate_tracks(url, tracks),
            #[cfg(target_os = "android")]
            Playback::Exo(_) => Ok(()),
        }
    }

    /// The current file's tracks, as of the last change. Never blocks.
    pub fn tracks(&self) -> Vec<Track> {
        match self {
            Playback::Mpv(p) => p.tracks(),
            #[cfg(target_os = "android")]
            Playback::Exo(e) => e.tracks(),
        }
    }
}

#[derive(Default)]
pub struct VideoHost {
    /// mpv, created when first needed (on a TV, only for files ExoPlayer can't play).
    player: RefCell<Option<Player>>,
    /// The id mpv's events carry.
    mpv_id: Cell<u64>,
    #[cfg(target_os = "android")]
    exo: RefCell<Option<crate::exo::Exo>>,
    #[cfg(target_os = "android")]
    exo_id: Cell<u64>,
    /// The player that plays now.
    engine: Cell<Option<Engine>>,
    renderer: RefCell<Option<VideoRenderer>>,
    gl: RefCell<Option<GlState>>,
    visible: Cell<bool>,
    unavailable: RefCell<Option<String>>,

    /// The current player decodes in compatibility (copy-back) mode.
    compat: Cell<bool>,
    /// The decoding mode a new mpv starts in (Settings).
    compat_setting: Cell<bool>,
    /// ExoPlayer works on this device (checked once at startup).
    exo_ok: Cell<bool>,
    /// Video was turned off because the GL surface went away (Android
    /// background, or a TV that rebuilds its window on a display-mode switch).
    /// Holds the video track that was playing, if the track list knew it.
    video_suspended: Cell<Option<Option<i64>>>,
}

static NEXT_PLAYER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A player event, to the controller on the UI thread.
fn forward_event(id: u64, event: PlayerEvent) {
    let _ = slint::invoke_from_event_loop(move || crate::app::with_controller(|c| c.on_player_event(id, event)));
}

/// ExoPlayer plays on this device (Android, with video on its own layer).
pub fn exo_available() -> bool {
    #[cfg(target_os = "android")]
    return crate::exo::available();
    #[cfg(not(target_os = "android"))]
    false
}

impl VideoHost {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Make mpv the player, with this new instance.
    pub fn set_player(&self, player: Result<(Player, u64), String>, compat: bool) {
        match player {
            Ok((player, id)) => {
                *self.player.borrow_mut() = Some(player);
                self.mpv_id.set(id);
                self.compat.set(compat);
                self.engine.set(Some(Engine::Mpv));
                self.show_mpv();
            }
            Err(reason) => {
                tracing::error!("player unavailable: {reason}");
                *self.unavailable.borrow_mut() = Some(reason);
            }
        }
    }

    /// Android: mpv shows video (its layer, and its surface).
    fn show_mpv(&self) {
        #[cfg(target_os = "android")]
        if let Some(player) = self.player.borrow().clone() {
            if crate::video_surface::enabled() {
                crate::exo::show_layer(false);
            }
            crate::video_surface::set_player(player);
        }
    }

    /// Switch to `engine` for the next playback, creating its player when
    /// needed, and stop the other one. Fails when that player can't start.
    pub fn use_engine(&self, engine: Engine, audio_session: Option<i32>) -> Result<(), String> {
        let have = match engine {
            Engine::Mpv => self.player.borrow().is_some(),
            #[cfg(target_os = "android")]
            Engine::Exo => self.exo.borrow().is_some(),
            #[cfg(not(target_os = "android"))]
            Engine::Exo => return Err("ExoPlayer only exists on Android".into()),
        };
        if self.engine.get() == Some(engine) && have {
            return Ok(());
        }
        // Whatever played before stops (its video layer goes away below).
        if let Some(old) = self.player() {
            let _ = old.stop();
        }
        match engine {
            Engine::Mpv => {
                if !have {
                    // Automatic decoding means compatibility (copy-back), as at startup.
                    let compat = self.compat_setting.get();
                    let created = Self::create_player(compat, audio_session)?;
                    *self.player.borrow_mut() = Some(created.0);
                    self.mpv_id.set(created.1);
                    self.compat.set(compat);
                }
                self.engine.set(Some(Engine::Mpv));
                self.show_mpv();
            }
            Engine::Exo => {
                #[cfg(target_os = "android")]
                {
                    if !have {
                        let id = NEXT_PLAYER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let exo = crate::exo::Exo::new(id, audio_session, concat!("Joshflix/", env!("CARGO_PKG_VERSION")), forward_event)
                            .map_err(|e| e.to_string())?;
                        tracing::info!("player {id}: ExoPlayer");
                        *self.exo.borrow_mut() = Some(exo);
                        self.exo_id.set(id);
                    }
                    crate::video_surface::clear_player();
                    crate::exo::show_layer(true);
                    self.engine.set(Some(Engine::Exo));
                }
            }
        }
        Ok(())
    }

    /// The decoding mode mpv starts with when it's created later.
    pub fn set_compat_setting(&self, compat: bool) {
        self.compat_setting.set(compat);
    }

    /// Whether ExoPlayer can play here ([`exo_available`] at startup), or
    /// stop being used for the rest of this run (`false`).
    pub fn set_exo_ok(&self, ok: bool) {
        self.exo_ok.set(ok);
    }

    pub fn exo_ok(&self) -> bool {
        self.exo_ok.get()
    }


    /// Create an mpv instance whose events go to the controller, tagged with an id.
    /// `audio_session` is the platform's audio session for mpv's AudioTrack output.
    pub fn create_player(compat_mode: bool, audio_session: Option<i32>) -> Result<(Player, u64), String> {
        let mut config = joshflix_player::PlayerConfig::for_platform();
        config.audio_session = audio_session;
        config.android_surface = surface_mode();
        if compat_mode && cfg!(target_os = "android") && std::env::var("JOSHFLIX_HWDEC").is_err() {
            config.hwdec = "mediacodec-copy,no".into();
        }
        let id = NEXT_PLAYER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("player {id}: mpv, hwdec={} compat={compat_mode} own surface={}", config.hwdec, config.android_surface);
        Player::new(&config, move |event| {
            // Log mpv's messages right here on its event thread, so they still
            // reach logcat and the relay when the UI thread is stuck.
            if let PlayerEvent::Log { level, text } = &event {
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
            forward_event(id, event);
        })
        .map(|player| (player, id))
        .map_err(|e| e.to_string())
    }

    /// Instance id of the current player; events from other instances are dropped.
    pub fn current_id(&self) -> u64 {
        match self.engine.get() {
            Some(Engine::Mpv) => self.mpv_id.get(),
            #[cfg(target_os = "android")]
            Some(Engine::Exo) => self.exo_id.get(),
            _ => 0,
        }
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
        self.mpv_id.set(id);
        self.compat.set(compat);
        self.video_suspended.set(None);
        self.engine.set(Some(Engine::Mpv));
        self.show_mpv();
    }

    /// The player that plays now.
    pub fn player(&self) -> Option<Playback> {
        match self.engine.get()? {
            Engine::Mpv => self.player.borrow().clone().map(Playback::Mpv),
            #[cfg(target_os = "android")]
            Engine::Exo => self.exo.borrow().clone().map(Playback::Exo),
            #[cfg(not(target_os = "android"))]
            Engine::Exo => None,
        }
    }

    /// The mpv instance, if one exists (it draws into the UI on desktop).
    fn mpv(&self) -> Option<Player> {
        self.player.borrow().clone()
    }

    /// Stop both players' threads (the app is closing).
    pub fn shutdown(&self) {
        if let Some(player) = self.mpv() {
            player.shutdown();
        }
        #[cfg(target_os = "android")]
        if let Some(exo) = self.exo.borrow_mut().take() {
            let _ = exo.stop();
        }
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
                    // With its own surface (Android), mpv draws video itself.
                    if host.renderer.borrow().is_none()
                        && !surface_mode()
                        && let Some(player) = host.mpv()
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
                    crate::perf::draw_started(crate::perf::now_ns());
                }
                RenderingState::AfterRendering => {
                    crate::perf::draw_finished(crate::perf::now_ns());
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
                        && let Some(player) = host.mpv()
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
                }
                _ => {}
            }
        });
        if let Err(e) = result {
            tracing::error!("rendering notifier unavailable: {e:?}");
        }
    }
}

/// mpv draws into its own surface under the UI instead of through [`VideoRenderer`].
pub fn surface_mode() -> bool {
    #[cfg(target_os = "android")]
    return crate::video_surface::enabled();
    #[cfg(not(target_os = "android"))]
    false
}

/// The player screen opened (`true`) or closed: with its own surface, mpv
/// holds it only while the player is open.
pub fn want_video(on: bool) {
    #[cfg(target_os = "android")]
    crate::video_surface::want_video(on);
    #[cfg(not(target_os = "android"))]
    let _ = on;
}

/// `player` is attached to mpv's own surface (own-surface mode).
pub fn shows_on_own_surface(player: &Player) -> bool {
    #[cfg(target_os = "android")]
    return crate::video_surface::is_attached(player);
    #[cfg(not(target_os = "android"))]
    {
        let _ = player;
        false
    }
}

/// Video has played on mpv's own surface for a while: that path works here.
pub fn confirm_own_surface() {
    #[cfg(target_os = "android")]
    crate::video_surface::video_worked();
}

/// Video never appeared on mpv's own surface: draw it into the UI for the
/// rest of this run ([`confirm_fallback`] makes it stick for this version).
pub fn give_up_own_surface() {
    #[cfg(target_os = "android")]
    crate::video_surface::give_up();
}

/// mpv's own surface exists and the player wants it (own-surface mode).
pub fn own_surface_ready() -> bool {
    #[cfg(target_os = "android")]
    return crate::video_surface::surface_ready();
    #[cfg(not(target_os = "android"))]
    false
}

/// After [`give_up_own_surface`], video still has to show when drawn into the UI.
pub fn fallback_pending() -> bool {
    #[cfg(target_os = "android")]
    return crate::video_surface::fallback_pending();
    #[cfg(not(target_os = "android"))]
    false
}

/// Video shows when drawn into the UI after a fallback: keep this version on
/// a single surface.
pub fn confirm_fallback() {
    #[cfg(target_os = "android")]
    crate::video_surface::fallback_worked();
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
