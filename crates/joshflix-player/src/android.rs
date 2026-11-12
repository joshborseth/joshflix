//! Android: give FFmpeg the JavaVM so MediaCodec decoding and AudioTrack work.

use std::ffi::{c_int, c_void};

/// Call once before creating the [`crate::Player`]. Looks for FFmpeg's
/// `av_jni_set_java_vm` in libmpv (static FFmpeg) or libavcodec (shared).
pub fn set_java_vm(vm: *mut c_void) -> bool {
    type SetVm = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    for name in ["libmpv.so", "libavcodec.so"] {
        // SAFETY: loading the bundled FFmpeg/libmpv; the symbol matches libavcodec/jni.h.
        unsafe {
            let Ok(lib) = libloading::Library::new(name) else { continue };
            if let Ok(set) = lib.get::<SetVm>(b"av_jni_set_java_vm\0") {
                let code = set(vm, std::ptr::null_mut());
                tracing::info!("av_jni_set_java_vm via {name}: {code}");
                // Keep the library loaded for the life of the process.
                std::mem::forget(lib);
                return code >= 0;
            }
        }
    }
    tracing::warn!("av_jni_set_java_vm not found; hardware decoding may fall back to software");
    false
}
