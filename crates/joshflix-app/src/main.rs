//! Desktop build, for development. Run at TV scale with
//! `SLINT_SCALE_FACTOR=2 cargo run -p joshflix-app` in a 1920×1080 window.

use std::rc::Rc;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,joshflix=debug".into()),
        )
        .init();
    if let Err(e) = joshflix_app::run(Rc::new(joshflix_app::platform::Desktop::new())) {
        eprintln!("joshflix: {e}");
        std::process::exit(1);
    }
}
