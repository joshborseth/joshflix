//! Minimal libmpv bindings (client API 2.x + render API), loaded at runtime.
//!
//! Only the handful of functions Joshflix uses are bound. Loading with
//! `libloading` means the app builds without libmpv present and can show a
//! friendly error if it's missing at runtime.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_double, c_int, c_void};

use libloading::Library;

pub type mpv_handle = c_void;
pub type mpv_render_context = c_void;

pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;

pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
pub const MPV_EVENT_SEEK: c_int = 20;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_STOP: c_int = 2;
pub const MPV_END_FILE_REASON_QUIT: c_int = 3;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
pub const MPV_RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;

pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

#[repr(C)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_property {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_end_file {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct mpv_event_log_message {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: c_int,
}

#[repr(C)]
pub struct mpv_render_param {
    pub type_: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_opengl_init_params {
    pub get_proc_address: unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct mpv_opengl_fbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

macro_rules! mpv_api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// Resolved libmpv entry points. The `Library` stays loaded for the struct's life.
        pub struct Mpv {
            _lib: Library,
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Mpv {
            /// # Safety
            /// `lib` must be a libmpv with client API 2.x.
            unsafe fn resolve(lib: Library) -> Result<Self, libloading::Error> {
                unsafe {
                    $(let $name = *lib.get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(concat!(stringify!($name), "\0").as_bytes())?;)*
                    Ok(Self { _lib: lib, $($name,)* })
                }
            }
        }
    };
}

mpv_api! {
    mpv_client_api_version: fn() -> u64;
    mpv_error_string: fn(c_int) -> *const c_char;
    mpv_free: fn(*mut c_void);
    mpv_create: fn() -> *mut mpv_handle;
    mpv_initialize: fn(*mut mpv_handle) -> c_int;
    mpv_terminate_destroy: fn(*mut mpv_handle);
    mpv_set_option_string: fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int;
    mpv_command: fn(*mut mpv_handle, *mut *const c_char) -> c_int;
    mpv_set_property: fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int;
    mpv_set_property_string: fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int;
    mpv_get_property: fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int;
    mpv_get_property_string: fn(*mut mpv_handle, *const c_char) -> *mut c_char;
    mpv_observe_property: fn(*mut mpv_handle, u64, *const c_char, c_int) -> c_int;
    mpv_request_log_messages: fn(*mut mpv_handle, *const c_char) -> c_int;
    mpv_wait_event: fn(*mut mpv_handle, c_double) -> *mut mpv_event;
    mpv_wakeup: fn(*mut mpv_handle);
    mpv_render_context_create: fn(*mut *mut mpv_render_context, *mut mpv_handle, *mut mpv_render_param) -> c_int;
    mpv_render_context_set_update_callback: fn(*mut mpv_render_context, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void);
    mpv_render_context_update: fn(*mut mpv_render_context) -> u64;
    mpv_render_context_render: fn(*mut mpv_render_context, *mut mpv_render_param) -> c_int;
    mpv_render_context_report_swap: fn(*mut mpv_render_context);
    mpv_render_context_free: fn(*mut mpv_render_context);
}

impl Mpv {
    /// Load libmpv from the usual names for this platform.
    pub fn load() -> Result<Self, String> {
        let candidates: &[&str] = if cfg!(target_os = "android") {
            &["libmpv.so"]
        } else if cfg!(target_os = "macos") {
            &["libmpv.2.dylib", "libmpv.dylib"]
        } else if cfg!(target_os = "windows") {
            &["libmpv-2.dll", "mpv-2.dll", "mpv-1.dll"]
        } else {
            &["libmpv.so.2", "libmpv.so"]
        };
        let mut errors = Vec::new();
        for name in candidates {
            // SAFETY: loading libmpv runs its (trivial) initializers only.
            match unsafe { Library::new(*name) } {
                Ok(lib) => {
                    // SAFETY: symbol signatures match mpv's client.h / render.h for API 2.x.
                    let mpv = unsafe { Self::resolve(lib) }.map_err(|e| format!("{name}: {e}"))?;
                    // SAFETY: plain getter.
                    let version = unsafe { (mpv.mpv_client_api_version)() };
                    if version >> 16 != 2 {
                        return Err(format!("{name}: unsupported client API {}.{}", version >> 16, version & 0xffff));
                    }
                    return Ok(mpv);
                }
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Err(format!("libmpv not found ({})", errors.join("; ")))
    }

    pub fn error_string(&self, code: c_int) -> String {
        // SAFETY: mpv_error_string returns a static string for any code.
        unsafe { std::ffi::CStr::from_ptr((self.mpv_error_string)(code)) }
            .to_string_lossy()
            .into_owned()
    }
}
