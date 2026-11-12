//! `joshflix-relay` binary: reads config from the environment and serves the
//! relay. See the library crate docs for the design.

use std::{net::SocketAddr, process::ExitCode, time::Duration};

use joshflix_relay::{AppState, Config, DEFAULT_BIND, router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        // journald adds its own timestamps; colors only help on a terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let bind = std::env::var("JOSHFLIX_RELAY_BIND")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let bind: SocketAddr = bind
        .parse()
        .map_err(|e| format!("JOSHFLIX_RELAY_BIND {bind:?} is not an address: {e}"))?;

    let updates = config.updates_dir();
    tokio::fs::create_dir_all(&updates)
        .await
        .map_err(|e| format!("cannot create {}: {e}", updates.display()))?;

    tracing::info!(
        %bind,
        data_dir = %config.data_dir.display(),
        rd_api_base = %config.rd_api_base,
        public_url = config.public_url.as_deref().unwrap_or("<from Host header>"),
        "starting joshflix-relay"
    );

    let state = AppState::new(config).map_err(|e| format!("cannot build HTTP client: {e}"))?;
    state.spawn_sweeper(Duration::from_secs(30));
    state.spawn_screen_watchdog();

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("cannot bind {bind}: {e}"))?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|e| format!("server error: {e}"))?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}
