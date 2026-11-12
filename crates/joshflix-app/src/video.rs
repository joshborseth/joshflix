//! Draws libmpv video underneath the Slint UI in the same OpenGL context.
//!
//! Slint clears the frame, calls our `BeforeRendering` hook (mpv renders the
//! current video frame into the default framebuffer), then draws the UI on
//! top. mpv's "new frame" callback asks Slint for a redraw.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::rc::Rc;

use joshflix_player::{Player, VideoRenderer};
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

/// GL calls used to hand a clean state back to Slint after mpv draws.
struct GlReset {
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    viewport: unsafe extern "C" fn(i32, i32, i32, i32),
    disable: unsafe extern "C" fn(u32),
    enable: unsafe extern "C" fn(u32),
    blend_func: unsafe extern "C" fn(u32, u32),
    active_texture: unsafe extern "C" fn(u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    bind_buffer: unsafe extern "C" fn(u32, u32),
    use_program: unsafe extern "C" fn(u32),
    pixel_store: unsafe extern "C" fn(u32, i32),
    bind_vertex_array: Option<unsafe extern "C" fn(u32)>,
}

impl GlReset {
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
        let vao = get(c"glBindVertexArray");
        Some(Self {
            bind_framebuffer: f!("glBindFramebuffer"),
            viewport: f!("glViewport"),
            disable: f!("glDisable"),
            enable: f!("glEnable"),
            blend_func: f!("glBlendFunc"),
            active_texture: f!("glActiveTexture"),
            bind_texture: f!("glBindTexture"),
            bind_buffer: f!("glBindBuffer"),
            use_program: f!("glUseProgram"),
            pixel_store: f!("glPixelStorei"),
            // SAFETY: as above; optional on GLES2.
            bind_vertex_array: (!vao.is_null()).then(|| unsafe { std::mem::transmute::<*const c_void, unsafe extern "C" fn(u32)>(vao) }),
        })
    }

    fn apply(&self, width: i32, height: i32) {
        const GL_FRAMEBUFFER: u32 = 0x8D40;
        const GL_SCISSOR_TEST: u32 = 0x0C11;
        const GL_DEPTH_TEST: u32 = 0x0B71;
        const GL_STENCIL_TEST: u32 = 0x0B90;
        const GL_CULL_FACE: u32 = 0x0B44;
        const GL_BLEND: u32 = 0x0BE2;
        const GL_ONE: u32 = 1;
        const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
        const GL_TEXTURE0: u32 = 0x84C0;
        const GL_TEXTURE_2D: u32 = 0x0DE1;
        const GL_ARRAY_BUFFER: u32 = 0x8892;
        const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
        const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
        // SAFETY: called with Slint's GL context current on the render thread.
        unsafe {
            if let Some(bind) = self.bind_vertex_array {
                bind(0);
            }
            (self.bind_framebuffer)(GL_FRAMEBUFFER, 0);
            (self.viewport)(0, 0, width, height);
            (self.disable)(GL_SCISSOR_TEST);
            (self.disable)(GL_DEPTH_TEST);
            (self.disable)(GL_STENCIL_TEST);
            (self.disable)(GL_CULL_FACE);
            (self.enable)(GL_BLEND);
            (self.blend_func)(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
            (self.active_texture)(GL_TEXTURE0);
            (self.bind_texture)(GL_TEXTURE_2D, 0);
            (self.bind_buffer)(GL_ARRAY_BUFFER, 0);
            (self.bind_buffer)(GL_ELEMENT_ARRAY_BUFFER, 0);
            (self.use_program)(0);
            (self.pixel_store)(GL_UNPACK_ALIGNMENT, 4);
        }
    }
}

#[derive(Default)]
pub struct VideoHost {
    player: RefCell<Option<Player>>,
    renderer: RefCell<Option<VideoRenderer>>,
    gl: RefCell<Option<GlReset>>,
    visible: Cell<bool>,
    unavailable: RefCell<Option<String>>,
}

impl VideoHost {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn set_player(&self, player: Result<Player, String>) {
        match player {
            Ok(player) => *self.player.borrow_mut() = Some(player),
            Err(reason) => {
                tracing::error!("player unavailable: {reason}");
                *self.unavailable.borrow_mut() = Some(reason);
            }
        }
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
                                *host.gl.borrow_mut() = GlReset::load(get_proc_address);
                            }
                            Err(e) => tracing::error!("video renderer: {e}"),
                        }
                    }
                    // mpv needs the render context before it can open any file, so it's
                    // created on the first frame; video is only drawn while visible.
                    if !host.visible.get() {
                        return;
                    }
                    if let Some(renderer) = host.renderer.borrow().as_ref() {
                        renderer.render(0, size.width as i32, size.height as i32);
                        if let Some(gl) = host.gl.borrow().as_ref() {
                            gl.apply(size.width as i32, size.height as i32);
                        }
                    }
                }
                RenderingState::AfterRendering => {
                    if host.visible.get()
                        && let Some(renderer) = host.renderer.borrow().as_ref()
                    {
                        renderer.report_swap();
                    }
                }
                RenderingState::RenderingTeardown => {
                    // The GL context is going away (e.g. Android pause): free GPU state now.
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
