//! Binary entrypoint for the Murklet image proxy service.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use murklet::cache::FsCache;
use murklet::http::{AppState, build_router};
use murklet::origin::{HttpOrigin, LocalFsOrigin};
use murklet::policy::{LogFormatConfig, MurkletConfig};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG_PATH: &str = "murklet.toml";

struct CliArgs {
    config_path: PathBuf,
    check_only: bool,
}

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
    let cli = parse_cli_args()?;
    let config = MurkletConfig::from_file(&cli.config_path)?;

    if cli.check_only {
        println!(
            "configuration valid: {} (bind={}, local_origin={}, http_origin={})",
            cli.config_path.display(),
            config.server.bind,
            config.origins.local.enabled,
            config.origins.http.enabled,
        );
        return Ok(());
    }

    init_tracing(config.server.log_format);

    let local_origin = if config.origins.local.enabled {
        Some(LocalFsOrigin::new(
            &config.origins.local.root,
            config.limits.max_input_bytes,
        )?)
    } else {
        None
    };

    let http_origin = config
        .http_origin_config()
        .map(HttpOrigin::new)
        .transpose()?;

    let cache = FsCache::new(&config.cache.dir, config.cache.max_entry_bytes)?;
    let request_signer = config.request_signer()?;

    let mut state = AppState::from_parts(local_origin, http_origin, cache)
        .with_max_concurrent_transforms(config.limits.max_concurrent_transforms);
    state.engine_limits = config.engine_limits();
    state.max_target_dimension = config.limits.max_target_dimension;
    state.cache_control = config.server.cache_control;
    state.accept_negotiation = config.server.accept_negotiation;
    state.metrics_enabled = config.server.metrics_enabled;
    state.cleanup_token = config.server.cleanup_token;
    state.request_signer = request_signer;
    state.diagnostic_headers = config.server.diagnostic_headers;

    let app = build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    tracing::info!(
        bind = %config.server.bind,
        local_origin = config.origins.local.enabled,
        http_origin = config.origins.http.enabled,
        metrics_enabled = config.server.metrics_enabled,
        "murklet server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn init_tracing(format: LogFormatConfig) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("murklet=info"));

    match format {
        LogFormatConfig::Pretty => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
        }
        LogFormatConfig::Json => {
            let _ = tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .try_init();
        }
    }
}

fn parse_cli_args() -> Result<CliArgs, String> {
    let mut check_only = false;
    let mut explicit_path: Option<PathBuf> = None;

    for arg in env::args().skip(1) {
        if arg == "--check" {
            check_only = true;
        } else if arg.starts_with('-') {
            return Err(format!("unrecognised flag '{arg}' (supported: --check)"));
        } else if explicit_path.is_none() {
            explicit_path = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
    }

    let config_path = explicit_path
        .or_else(|| {
            env::var("MURKLET_CONFIG")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    Ok(CliArgs {
        config_path,
        check_only,
    })
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
