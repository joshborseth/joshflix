//! GPU time of each frame's UI drawing, from GL timer queries
//! (`GL_EXT_disjoint_timer_query`). It tells a frame that's slow because the
//! GPU can't keep up from one that's slow on the CPU. Results are read a few
//! frames later, without waiting for the GPU.

use std::collections::VecDeque;
use std::ffi::{CStr, c_void};

const GL_EXTENSIONS: u32 = 0x1F03;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_TIME_ELAPSED_EXT: u32 = 0x88BF;
const GL_QUERY_RESULT_EXT: u32 = 0x8866;
const GL_QUERY_RESULT_AVAILABLE_EXT: u32 = 0x8867;
const GL_GPU_DISJOINT_EXT: u32 = 0x8FBB;

/// Queries in flight at most; past that a frame goes unmeasured rather than wait.
const MAX_PENDING: usize = 6;

#[allow(clippy::type_complexity)]
struct Fns {
    gen_queries: unsafe extern "C" fn(i32, *mut u32),
    delete_queries: unsafe extern "C" fn(i32, *const u32),
    begin_query: unsafe extern "C" fn(u32, u32),
    end_query: unsafe extern "C" fn(u32),
    get_query_objectuiv: unsafe extern "C" fn(u32, u32, *mut u32),
    get_query_objectui64v: unsafe extern "C" fn(u32, u32, *mut u64),
    get_integerv: unsafe extern "C" fn(u32, *mut i32),
}

pub struct GpuTimer {
    f: Fns,
    pending: VecDeque<u32>,
    free: Vec<u32>,
    active: Option<u32>,
}

/// Look up a GL entry point; `None` when the driver doesn't have it.
fn symbol(get: &dyn Fn(&CStr) -> *const c_void, name: &CStr) -> Option<*const c_void> {
    let ptr = get(name);
    (!ptr.is_null()).then_some(ptr)
}

/// A GL string such as the renderer name, or "" when unavailable.
///
/// # Safety
/// A GL context must be current and `get_string` must be `glGetString`.
unsafe fn gl_string(get_string: unsafe extern "C" fn(u32) -> *const u8, name: u32) -> String {
    // SAFETY: per the caller; GL returns a static NUL-terminated string or null.
    let ptr = unsafe { get_string(name) };
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null GL strings are NUL-terminated.
    unsafe { CStr::from_ptr(ptr.cast()) }.to_string_lossy().into_owned()
}

impl GpuTimer {
    /// Set up timer queries if the driver supports them, and log the GPU.
    ///
    /// # Safety
    /// The GL context `get` belongs to must be current.
    pub unsafe fn new(get: &dyn Fn(&CStr) -> *const c_void) -> Option<Self> {
        let get_string: unsafe extern "C" fn(u32) -> *const u8 =
            // SAFETY: glGetString has this signature.
            unsafe { std::mem::transmute(symbol(get, c"glGetString")?) };
        // SAFETY: the context is current (caller).
        let (renderer, version, extensions) = unsafe {
            (gl_string(get_string, GL_RENDERER), gl_string(get_string, GL_VERSION), gl_string(get_string, GL_EXTENSIONS))
        };
        let supported = extensions.split_ascii_whitespace().any(|e| e == "GL_EXT_disjoint_timer_query");
        tracing::info!("GPU: {renderer}; {version}; frame timing {}", if supported { "on" } else { "unavailable" });
        if !supported {
            return None;
        }
        macro_rules! f {
            ($name:literal) => {
                // SAFETY: the symbol is the GL entry point with the declared signature.
                unsafe { std::mem::transmute::<*const c_void, _>(symbol(get, $name)?) }
            };
        }
        Some(Self {
            f: Fns {
                gen_queries: f!(c"glGenQueriesEXT"),
                delete_queries: f!(c"glDeleteQueriesEXT"),
                begin_query: f!(c"glBeginQueryEXT"),
                end_query: f!(c"glEndQueryEXT"),
                get_query_objectuiv: f!(c"glGetQueryObjectuivEXT"),
                get_query_objectui64v: f!(c"glGetQueryObjectui64vEXT"),
                get_integerv: f!(c"glGetIntegerv"),
            },
            pending: VecDeque::new(),
            free: Vec::new(),
            active: None,
        })
    }

    /// Start timing the UI's drawing. Call with the context current, and not
    /// while another `GL_TIME_ELAPSED` query (mpv's) is running.
    pub fn begin(&mut self) {
        self.collect();
        if self.active.is_some() || self.pending.len() >= MAX_PENDING {
            return;
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                let mut id = 0;
                // SAFETY: context current (caller); writes one query name.
                unsafe { (self.f.gen_queries)(1, &mut id) };
                id
            }
        };
        // SAFETY: as above.
        unsafe { (self.f.begin_query)(GL_TIME_ELAPSED_EXT, id) };
        self.active = Some(id);
    }

    /// Stop timing; the result is collected on a later frame.
    pub fn end(&mut self) {
        if let Some(id) = self.active.take() {
            // SAFETY: context current; the query `id` is active.
            unsafe { (self.f.end_query)(GL_TIME_ELAPSED_EXT) };
            self.pending.push_back(id);
        }
    }

    /// Hand finished results to the frame stats, oldest first, without blocking.
    fn collect(&mut self) {
        let mut done = Vec::new();
        while let Some(&id) = self.pending.front() {
            let mut available = 0u32;
            // SAFETY: context current; `id` names a query that has ended.
            unsafe { (self.f.get_query_objectuiv)(id, GL_QUERY_RESULT_AVAILABLE_EXT, &mut available) };
            if available == 0 {
                break;
            }
            let mut ns = 0u64;
            // SAFETY: as above; the result is available.
            unsafe { (self.f.get_query_objectui64v)(id, GL_QUERY_RESULT_EXT, &mut ns) };
            self.pending.pop_front();
            self.free.push(id);
            done.push(ns);
        }
        let mut disjoint = 0i32;
        // SAFETY: context current. Reading the flag also clears it.
        unsafe { (self.f.get_integerv)(GL_GPU_DISJOINT_EXT, &mut disjoint) };
        // A disjoint event (frequency change, power state) makes these times meaningless.
        if disjoint == 0 {
            for ns in done {
                crate::perf::gpu_frame(i64::try_from(ns).unwrap_or(i64::MAX));
            }
        }
    }
}

impl Drop for GpuTimer {
    fn drop(&mut self) {
        // Dropped from RenderingTeardown, while the context is still current.
        let ids: Vec<u32> = self.free.drain(..).chain(self.pending.drain(..)).chain(self.active.take()).collect();
        if !ids.is_empty() {
            // SAFETY: context current; these are our query names.
            unsafe { (self.f.delete_queries)(ids.len() as i32, ids.as_ptr()) };
        }
    }
}
