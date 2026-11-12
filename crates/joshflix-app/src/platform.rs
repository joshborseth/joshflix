//! Platform services: storage paths, display capabilities, frame-rate
//! switching, keeping the screen on and installing updates.

use std::path::PathBuf;

use joshflix_core::release::DisplayCaps;

pub trait Platform {
    /// Private, persistent app data (database).
    fn data_dir(&self) -> PathBuf;
    /// Disposable cache (artwork, downloads).
    fn cache_dir(&self) -> PathBuf;
    /// What the connected display can show.
    fn display_caps(&self) -> DisplayCaps;
    /// Ask the display to switch to a refresh rate matching `fps` (0 = reset).
    fn set_frame_rate(&self, fps: f64);
    /// Keep the screen awake during playback.
    fn keep_screen_on(&self, on: bool);
    /// Hand a verified APK to the system installer.
    /// Hand a downloaded APK to the system installer. Runs in the background;
    /// `done` is called on that background thread with the outcome.
    fn install_apk(&self, path: &std::path::Path, done: InstallDone);
    /// True when this platform can install updates.
    fn can_install_updates(&self) -> bool;
    /// Human-readable device name for About.
    fn device_label(&self) -> String;
}

pub type InstallDone = Box<dyn FnOnce(Result<(), String>) + Send + 'static>;

/// The Android ABI of the running build (the update must match it).
pub fn current_abi() -> &'static str {
    match std::env::consts::ARCH {
        "arm" => "armeabi-v7a",
        "aarch64" => "arm64-v8a",
        "x86_64" => "x86_64",
        "x86" => "x86",
        other => other,
    }
}

/// Desktop (development) platform.
pub struct Desktop {
    base: PathBuf,
}

impl Desktop {
    pub fn new() -> Self {
        let base = std::env::var_os("JOSHFLIX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("joshflix")))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/joshflix")))
            .unwrap_or_else(|| PathBuf::from(".joshflix"));
        Self { base }
    }
}

impl Platform for Desktop {
    fn data_dir(&self) -> PathBuf {
        self.base.join("data")
    }

    fn cache_dir(&self) -> PathBuf {
        self.base.join("cache")
    }

    fn display_caps(&self) -> DisplayCaps {
        // JOSHFLIX_DISPLAY=sdr1080 simulates a basic TV during development.
        match std::env::var("JOSHFLIX_DISPLAY").as_deref() {
            Ok("sdr1080") => DisplayCaps::SDR_1080,
            _ => DisplayCaps::EVERYTHING,
        }
    }

    fn set_frame_rate(&self, fps: f64) {
        tracing::debug!("frame-rate match requested: {fps:.3} (no-op on desktop)");
    }

    fn keep_screen_on(&self, _on: bool) {}

    fn install_apk(&self, path: &std::path::Path, done: InstallDone) {
        done(Err(format!("Updates install on the TV. Downloaded to {}", path.display())));
    }

    fn can_install_updates(&self) -> bool {
        false
    }

    fn device_label(&self) -> String {
        format!("Desktop ({})", std::env::consts::OS)
    }
}

#[cfg(target_os = "android")]
pub mod android {
    //! Android TV implementation over JNI and the NDK.

    use std::cell::Cell;
    use std::ffi::c_void;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use jni::JavaVM;
    use jni::objects::{JObject, JValue};
    use joshflix_core::release::DisplayCaps;
    use android_activity::{AndroidApp, WindowManagerFlags};

    use super::Platform;

    pub struct Android {
        app: AndroidApp,
        vm: JavaVM,
        caps: DisplayCaps,
        /// Bumped by every frame-rate change, so a pending UI request can tell it's stale.
        frame_rate_generation: Rc<Cell<u64>>,
    }

    /// How long the display gets to switch back from a video's mode before the UI asks for its own.
    const UI_RATE_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

    impl Android {
        pub fn new(app: AndroidApp) -> Self {
            // SAFETY: android-activity hands us valid JavaVM and Activity pointers for the process lifetime.
            let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.expect("JavaVM");
            let mut android = Self { app, vm, caps: DisplayCaps::SDR_1080, frame_rate_generation: Rc::default() };
            android.caps = android.query_caps().unwrap_or_else(|e| {
                tracing::warn!("display caps: {e}");
                DisplayCaps::SDR_1080
            });
            tracing::info!("display caps: {:?}", android.caps);
            android
        }

        pub fn vm_ptr(&self) -> *mut c_void {
            self.app.vm_as_ptr()
        }

        fn activity(&self) -> JObject<'static> {
            // SAFETY: the activity global ref outlives the app.
            unsafe { JObject::from_raw(self.app.activity_as_ptr().cast()) }
        }

        fn query_caps(&self) -> Result<DisplayCaps, jni::errors::Error> {
            let mut env = self.vm.attach_current_thread()?;
            let activity = self.activity();
            let wm = env
                .call_method(&activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?
                .l()?;
            let display = env.call_method(&wm, "getDefaultDisplay", "()Landroid/view/Display;", &[])?.l()?;

            // Tallest supported mode.
            let modes = env
                .call_method(&display, "getSupportedModes", "()[Landroid/view/Display$Mode;", &[])?
                .l()?;
            let modes = jni::objects::JObjectArray::from(modes);
            let mut max_height = 0;
            for i in 0..env.get_array_length(&modes)? {
                let mode = env.get_object_array_element(&modes, i)?;
                let w = env.call_method(&mode, "getPhysicalWidth", "()I", &[])?.i()?;
                let h = env.call_method(&mode, "getPhysicalHeight", "()I", &[])?.i()?;
                max_height = max_height.max(w.min(h));
            }

            // HDR types: 1 Dolby Vision, 2 HDR10, 3 HLG, 4 HDR10+.
            let caps = env
                .call_method(&display, "getHdrCapabilities", "()Landroid/view/Display$HdrCapabilities;", &[])?
                .l()?;
            let mut hdr = [false; 5];
            if !caps.is_null() {
                let types = env.call_method(&caps, "getSupportedHdrTypes", "()[I", &[])?.l()?;
                let types = jni::objects::JIntArray::from(types);
                let len = env.get_array_length(&types)? as usize;
                let mut buf = vec![0i32; len];
                env.get_int_array_region(&types, 0, &mut buf)?;
                for t in buf {
                    if let Some(slot) = hdr.get_mut(t as usize) {
                        *slot = true;
                    }
                }
            }
            Ok(DisplayCaps {
                max_height: if max_height >= 2000 { 2160 } else if max_height >= 1000 { 1080 } else { 720 },
                dolby_vision: hdr[1],
                hdr10: hdr[2] || hdr[4],
                hlg: hdr[3],
                hdr10_plus: hdr[4],
            })
        }

        fn sdk_int(&self) -> i32 {
            let Ok(mut env) = self.vm.attach_current_thread() else { return 0 };
            env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
                .and_then(|v| v.i())
                .unwrap_or(0)
        }

        fn files_dir(&self, method: &str) -> Option<PathBuf> {
            let mut env = self.vm.attach_current_thread().ok()?;
            let activity = self.activity();
            let file = env.call_method(&activity, method, "()Ljava/io/File;", &[]).ok()?.l().ok()?;
            let path = env.call_method(&file, "getAbsolutePath", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
            let path: String = env.get_string(&path.into()).ok()?.into();
            Some(PathBuf::from(path))
        }
    }

    /// Copy the APK into a MediaStore Downloads entry we own (API 29+, no storage
    /// permission) and open it with the system installer. Runs on a background thread.
    fn install_on_thread(vm: &JavaVM, activity: &JObject<'static>, path: &Path) -> Result<(), String> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).map_err(|e| format!("can't read the download: {e}"))?;
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let result = (|| -> Result<(), jni::errors::Error> {
            let activity = activity;
            let resolver = env
                .call_method(&activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
                .l()?;

            // MediaStore.Downloads entry we own (API 29+): no storage permission needed.
            let values = env.new_object("android/content/ContentValues", "()V", &[])?;
            let put = |env: &mut jni::JNIEnv, key: &str, value: &str| -> Result<(), jni::errors::Error> {
                let k = env.new_string(key)?;
                let v = env.new_string(value)?;
                env.call_method(&values, "put", "(Ljava/lang/String;Ljava/lang/String;)V", &[JValue::Object(&k), JValue::Object(&v)])?;
                Ok(())
            };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("joshflix-update.apk");
            put(&mut env, "_display_name", name)?;
            put(&mut env, "mime_type", "application/vnd.android.package-archive")?;
            let collection = env
                .get_static_field("android/provider/MediaStore$Downloads", "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?
                .l()?;
            let uri = env
                .call_method(
                    &resolver,
                    "insert",
                    "(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;",
                    &[JValue::Object(&collection), JValue::Object(&values)],
                )?
                .l()?;
            if uri.is_null() {
                return Err(jni::errors::Error::NullPtr("MediaStore insert"));
            }
            let stream = env
                .call_method(&resolver, "openOutputStream", "(Landroid/net/Uri;)Ljava/io/OutputStream;", &[JValue::Object(&uri)])?
                .l()?;
            // Stream the file in chunks instead of holding the whole APK in memory.
            let mut chunk = vec![0u8; 256 * 1024];
            loop {
                let n = file.read(&mut chunk).map_err(|_| jni::errors::Error::JavaException)?;
                if n == 0 {
                    break;
                }
                let array = env.byte_array_from_slice(&chunk[..n])?;
                env.call_method(&stream, "write", "([B)V", &[JValue::Object(&array)])?;
                env.delete_local_ref(array)?;
            }
            env.call_method(&stream, "close", "()V", &[])?;

            let action = env.new_string("android.intent.action.VIEW")?;
            let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
            let mime = env.new_string("application/vnd.android.package-archive")?;
            env.call_method(
                &intent,
                "setDataAndType",
                "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
                &[JValue::Object(&uri), JValue::Object(&mime)],
            )?;
            // FLAG_GRANT_READ_URI_PERMISSION | FLAG_ACTIVITY_NEW_TASK
            env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x0000_0001 | 0x1000_0000)])?;
            env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
            Ok(())
        })();
        result.map_err(|e| {
            // Clear any pending Java exception so later JNI calls on this thread work.
            let _ = env.exception_clear();
            e.to_string()
        })
    }

    /// The fastest refresh rate the display offers at its current resolution.
    fn max_refresh_rate(app: &AndroidApp) -> Result<(f32, f32), jni::errors::Error> {
        // SAFETY: android-activity hands us valid JavaVM and Activity pointers for the process lifetime.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let result = (|| {
            let wm = env.call_method(&activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?.l()?;
            let display = env.call_method(&wm, "getDefaultDisplay", "()Landroid/view/Display;", &[])?.l()?;
            let size = |env: &mut jni::JNIEnv, mode: &JObject| -> Result<(i32, i32, f32), jni::errors::Error> {
                Ok((
                    env.call_method(mode, "getPhysicalWidth", "()I", &[])?.i()?,
                    env.call_method(mode, "getPhysicalHeight", "()I", &[])?.i()?,
                    env.call_method(mode, "getRefreshRate", "()F", &[])?.f()?,
                ))
            };
            let current = env.call_method(&display, "getMode", "()Landroid/view/Display$Mode;", &[])?.l()?;
            let (width, height, current_rate) = size(&mut env, &current)?;
            let modes = env.call_method(&display, "getSupportedModes", "()[Landroid/view/Display$Mode;", &[])?.l()?;
            let modes = jni::objects::JObjectArray::from(modes);
            let mut best = current_rate;
            for i in 0..env.get_array_length(&modes)? {
                let mode = env.get_object_array_element(&modes, i)?;
                let (w, h, rate) = size(&mut env, &mode)?;
                if (w, h) == (width, height) {
                    best = best.max(rate);
                }
            }
            Ok((current_rate, best))
        })();
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result
    }

    /// Ask for the display's fastest refresh rate while showing the UI, so
    /// animations get more frames on 90/120 Hz screens. Only seamless switches
    /// are allowed: a TV that would blank its screen to change mode stays put.
    pub fn request_ui_refresh_rate(app: &AndroidApp) {
        let Some(window) = app.native_window() else { return };
        let (current, best) = match max_refresh_rate(app) {
            Ok(rates) => rates,
            Err(e) => {
                tracing::warn!("display modes: {e}");
                return;
            }
        };
        type WithStrategy = unsafe extern "C" fn(*mut c_void, f32, i8, i8) -> i32;
        type Simple = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
        // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT = 0, CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS = 0.
        // A vote of 0 Hz when nothing faster exists clears any earlier video vote.
        let rate = if best > current + 1.0 { best } else { 0.0 };
        // Looked up at runtime: Android < 11 has neither, and keeps its mode.
        // SAFETY: libandroid is always present; symbol types match the NDK headers.
        unsafe {
            let Ok(lib) = libloading::Library::new("libandroid.so") else { return };
            let code = if let Ok(f) = lib.get::<WithStrategy>(b"ANativeWindow_setFrameRateWithChangeStrategy\0") {
                f(window.ptr().as_ptr().cast(), rate, 0, 0)
            } else if let Ok(f) = lib.get::<Simple>(b"ANativeWindow_setFrameRate\0") {
                // Android 11 only switches seamlessly here anyway.
                f(window.ptr().as_ptr().cast(), rate, 0)
            } else {
                return;
            };
            tracing::info!("UI refresh rate: display at {current:.1} Hz, fastest {best:.1} Hz, asked for {rate:.1} ({code})");
        }
    }

    impl Platform for Android {
        fn data_dir(&self) -> PathBuf {
            self.app
                .internal_data_path()
                .or_else(|| self.files_dir("getFilesDir"))
                .unwrap_or_else(|| PathBuf::from("/data/local/tmp/joshflix"))
        }

        fn cache_dir(&self) -> PathBuf {
            self.files_dir("getCacheDir").unwrap_or_else(|| self.data_dir().join("cache"))
        }

        fn display_caps(&self) -> DisplayCaps {
            self.caps
        }

        fn set_frame_rate(&self, fps: f64) {
            let generation = self.frame_rate_generation.get() + 1;
            self.frame_rate_generation.set(generation);
            if fps <= 0.0 {
                // Clearing the video's vote lets the display return to its usual
                // mode. Once it has, ask for the UI's fastest seamless rate.
                let (app, current) = (self.app.clone(), self.frame_rate_generation.clone());
                slint::Timer::single_shot(UI_RATE_DELAY, move || {
                    if current.get() == generation {
                        request_ui_refresh_rate(&app);
                    }
                });
            }
            let Some(window) = self.app.native_window() else { return };
            type WithStrategy = unsafe extern "C" fn(*mut c_void, f32, i8, i8) -> i32;
            type Simple = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
            // Looked up at runtime so the app still loads on Android < 11.
            // SAFETY: libandroid is always present; symbol types match the NDK headers.
            unsafe {
                let Ok(lib) = libloading::Library::new("libandroid.so") else { return };
                // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = 1, CHANGE_FRAME_RATE_ALWAYS = 1
                if let Ok(f) = lib.get::<WithStrategy>(b"ANativeWindow_setFrameRateWithChangeStrategy\0") {
                    let code = f(window.ptr().as_ptr().cast(), fps as f32, 1, 1);
                    tracing::info!("setFrameRateWithChangeStrategy({fps:.3}) = {code}");
                } else if let Ok(f) = lib.get::<Simple>(b"ANativeWindow_setFrameRate\0") {
                    let code = f(window.ptr().as_ptr().cast(), fps as f32, 1);
                    tracing::info!("setFrameRate({fps:.3}) = {code}");
                }
            }
        }

        fn keep_screen_on(&self, on: bool) {
            if on {
                self.app.set_window_flags(WindowManagerFlags::KEEP_SCREEN_ON, WindowManagerFlags::empty());
            } else {
                self.app.set_window_flags(WindowManagerFlags::empty(), WindowManagerFlags::KEEP_SCREEN_ON);
            }
        }

        fn install_apk(&self, path: &Path, done: super::InstallDone) {
            if self.sdk_int() < 29 {
                done(Err("This TV's Android version can't install updates from the app. Install it with adb.".into()));
                return;
            }
            // Copying a 40 MB APK through JNI takes seconds; keep it off the UI thread.
            let (vm_ptr, activity_ptr) = (self.app.vm_as_ptr() as usize, self.app.activity_as_ptr() as usize);
            let path = path.to_owned();
            std::thread::Builder::new()
                .name("apk-install".into())
                .spawn(move || {
                    // SAFETY: the JavaVM and the activity's global ref live for the whole process.
                    let result = unsafe { JavaVM::from_raw(vm_ptr as *mut jni::sys::JavaVM) }
                        .map_err(|e| e.to_string())
                        .and_then(|vm| {
                            let activity = unsafe { JObject::from_raw(activity_ptr as jni::sys::jobject) };
                            install_on_thread(&vm, &activity, &path)
                        });
                    done(result.map_err(|e| {
                        format!("Couldn't open the installer ({e}). Allow Joshflix to install unknown apps in Android settings.")
                    }));
                })
                .expect("spawn installer thread");
        }

        fn can_install_updates(&self) -> bool {
            true
        }

        fn device_label(&self) -> String {
            let Ok(mut env) = self.vm.attach_current_thread() else { return "Android TV".into() };
            let read = |env: &mut jni::JNIEnv, field: &str| -> Option<String> {
                let value = env.get_static_field("android/os/Build", field, "Ljava/lang/String;").ok()?.l().ok()?;
                env.get_string(&value.into()).ok().map(Into::into)
            };
            let maker = read(&mut env, "MANUFACTURER").unwrap_or_default();
            let model = read(&mut env, "MODEL").unwrap_or_default();
            format!("{maker} {model} · Android {}", self.sdk_int()).trim().to_owned()
        }
    }
}
