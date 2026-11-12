//! Joshflix: a native Rust TV app. Slint draws the UI, libmpv plays video in
//! the same OpenGL context, Comet finds Real-Debrid sources and Cinemeta
//! supplies titles and artwork.

slint::include_modules!();

mod app;
mod format;
mod keyboard;
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
use joshflix_player::{Player, PlayerConfig};
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

/// Build everything and run the UI until the window closes.
pub fn run(platform: Rc<dyn Platform>) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("joshflix-io")
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;

    let ui = AppWindow::new().map_err(|e| format!("window: {e}"))?;
    let http = Http::new().map_err(|e| e.to_string())?;
    let store = Store::open(&platform.data_dir().join("joshflix.redb")).map_err(|e| format!("storage: {e}"))?;
    let server = std::env::var("JOSHFLIX_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
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
    let player = Player::new(&PlayerConfig::for_platform(), |event| {
        let _ = slint::invoke_from_event_loop(move || with_controller(|c| c.on_player_event(event)));
    });
    video.set_player(player.map_err(|e| e.to_string()));
    video.attach(&ui);

    AppState::get(&ui).on_key(|key| {
        let mut handled = false;
        with_controller(|c| handled = c.on_key(&key));
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
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("joshflix"),
    );
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    if let Err(e) = slint::android::init(app.clone()) {
        log::error!("slint init: {e}");
        return;
    }
    let platform = Rc::new(platform::android::Android::new(app));
    joshflix_player::android::set_java_vm(platform.vm_ptr());
    if let Err(e) = run(platform) {
        log::error!("joshflix: {e}");
    }
}
