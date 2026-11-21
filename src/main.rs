//! Binary entrypoint for the Murklet image proxy service.

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use murklet::cache::FsCache;
use murklet::engine::DEFAULT_MAX_INPUT_BYTES;
use murklet::http::{AppState, build_router};
use murklet::origin::LocalFsOrigin;

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:9876";
const DEFAULT_ORIGIN_DIR: &str = "./assets";
const DEFAULT_CACHE_DIR: &str = "./cache";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("murklet: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let bind_addr: SocketAddr = env::var("MURKLET_BIND_ADDR")
        .unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_owned())
        .parse()?;
    let origin_dir =
        env::var("MURKLET_ORIGIN_DIR").unwrap_or_else(|_| DEFAULT_ORIGIN_DIR.to_owned());
    let cache_dir = env::var("MURKLET_CACHE_DIR").unwrap_or_else(|_| DEFAULT_CACHE_DIR.to_owned());

    let origin = LocalFsOrigin::new(&origin_dir, DEFAULT_MAX_INPUT_BYTES)?;
    let cache = FsCache::new(&cache_dir, DEFAULT_MAX_INPUT_BYTES)?;

    let mut state = AppState::new(origin, cache);
    state.cleanup_token = env::var("MURKLET_CLEANUP_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    state.diagnostic_headers = env::var("MURKLET_DIAGNOSTIC_HEADERS")
        .ok()
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1");

    let app = build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    eprintln!("murklet listening on http://{bind_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
