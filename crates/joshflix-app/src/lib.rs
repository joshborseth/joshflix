//! Joshflix: a native Rust TV app. Slint draws the UI, libmpv plays video in
//! the same OpenGL context, Comet finds Real-Debrid sources and Cinemeta
//! supplies titles and artwork.

slint::include_modules!();

#[cfg(target_os = "android")]
mod android_platform;
#[cfg(target_os = "android")]
mod vsync;
mod app;
#[cfg(target_os = "android")]
mod audio_fx;
mod format;
mod keyboard;
mod logsink;
pub mod platform;
mod video;

use std::rc::Rc;

use joshflix_core::cinemeta::Cinemeta;
use joshflix_core::http::Http;
use joshflix_core::images::ImageLoader;
use joshflix_core::pairing::Relay;
use joshflix_core::store::Store;
use joshflix_core::subtitles::OpenSubtitles;
use joshflix_core::update::Updater;
use slint::ComponentHandle;

use crate::app::{Controller, Services, with_controller};
use crate::platform::Platform;
use crate::video::VideoHost;

/// The private relay on the dev box (pairing + updates). Override at build
/// time with `JOSHFLIX_SERVER_URL`, or at runtime on desktop with the same env var.
pub const DEFAULT_SERVER: &str = match option_env!("JOSHFLIX_SERVER_URL") {
    Some(url) => url,
    None => "http://192.168.1.67:8788",
};

/// Development overrides from `<data dir>/dev.env` (`JOSHFLIX_*=value` lines).
/// The file lives in app-private storage, so on Android only adb root or
/// run-as can create it. Must run before any other thread starts.
fn load_dev_env(dir: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(dir.join("dev.env")) else { return };
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key.starts_with("JOSHFLIX_") {
                tracing::info!("dev override: {key}");
                // SAFETY: called at startup before the runtime or player threads exist.
                unsafe { std::env::set_var(key, value.trim()) };
            }
        }
    }
}

/// Build everything and run the UI until the window closes.
pub fn run(platform: Rc<dyn Platform>) -> Result<(), String> {
    load_dev_env(&platform.data_dir());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("joshflix-io")
        // Artwork decodes run here; on a TV's few slow cores they must not
        // take time from the UI thread while it animates.
        .on_thread_start(platform::lower_thread_priority)
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;

    let ui = AppWindow::new().map_err(|e| format!("window: {e}"))?;
    let http = Http::new().map_err(|e| e.to_string())?;
    let store = Store::open(&platform.data_dir().join("joshflix.redb")).map_err(|e| format!("storage: {e}"))?;
    let server = std::env::var("JOSHFLIX_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    // TVs usually have no adb: their logs go to the relay on the dev box.
    if cfg!(target_os = "android") {
        logsink::spawn_uploader(runtime.handle(), http.clone(), &server, &platform.device_label());
    }
    let services = Services {
        cinemeta: Cinemeta::new(http.clone()),
        subtitles: OpenSubtitles::new(http.clone()),
        images: ImageLoader::new(http.clone(), platform.cache_dir().join("artwork")),
        relay: Relay::new(http.clone(), server.clone()),
        updater: Updater::new(http.clone(), server),
        store,
        http,
    };

    let video = VideoHost::new();
    // Automatic means compatibility (MediaCodec copy-back): it's what mpv-android
    // uses by default, and zero-copy through AImageReader stalled on the only
    // Android device it was tried on. "Hardware" in Settings opts into zero-copy.
    let compat = services.store.settings().ok().and_then(|s| s.video_compat_mode).unwrap_or(true);
    video.set_player(VideoHost::create_player(compat, platform.audio_session()), compat);
    video.attach(&ui);

    AppState::get(&ui).on_key(|key| {
        let mut handled = false;
        with_controller(|c| handled = c.on_key(&key));
        #[cfg(target_os = "android")]
        android_platform::report_key_handled(handled);
        handled
    });

    app::install(Controller::new(ui.clone_strong(), services, runtime.handle().clone(), platform, video));
    with_controller(Controller::start);
    let result = ui.run().map_err(|e| format!("event loop: {e}"));
    app::uninstall();
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    // logcat, plus the buffer that `logsink` uploads to the relay.
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("joshflix"),
    );
    if log::set_logger(Box::leak(Box::new(logsink::TeeLogger { logcat }))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    let tv = android_platform::TvPlatform::new(
        app.clone(),
        Box::new(|key| {
            let mut handled = false;
            with_controller(|c| handled = c.on_key(key));
            handled
        }),
        Box::new(|visible| with_controller(|c| c.on_app_visibility(visible))),
    );
    match tv.map(|tv| slint::platform::set_platform(Box::new(tv))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            log::error!("set_platform: {e}");
            return;
        }
        Err(e) => {
            log::error!("platform: {e}");
            return;
        }
    }
    let platform = Rc::new(platform::android::Android::new(app));
    joshflix_player::android::set_java_vm(platform.vm_ptr());
    if let Err(e) = run(platform) {
        log::error!("joshflix: {e}");
    }
}
