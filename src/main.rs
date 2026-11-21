//! Binary entrypoint for the Murklet image proxy service.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use murklet::cache::FsCache;
use murklet::engine::WatermarkRegistry;
use murklet::http::{AppState, build_router, explain_load_request};
use murklet::origin::{HttpOrigin, LocalFsOrigin, S3Origin};
use murklet::policy::{LogFormatConfig, MurkletConfig};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG_PATH: &str = "murklet.toml";

enum CliCommand {
    Serve {
        config_path: PathBuf,
    },
    Check {
        config_path: PathBuf,
    },
    Explain {
        target: String,
        config_path: PathBuf,
        accept: Option<String>,
    },
    Sign {
        target: String,
        config_path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("murklet: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    match parse_cli_args()? {
        CliCommand::Check { config_path } => {
            let config = MurkletConfig::from_file(&config_path)?;
            println!(
                "configuration valid: {} (bind={}, local_origin={}, http_origin={}, presets={}, watermarks={})",
                config_path.display(),
                config.server.bind,
                config.origins.local.enabled,
                config.origins.http.enabled,
                config.presets.len(),
                config.watermarks.len(),
            );
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Explain {
            target,
            config_path,
            accept,
        } => {
            let config = MurkletConfig::from_file(&config_path)?;
            let report = explain_load_request(&config, &target, accept.as_deref());
            println!("{report}");
            if report.is_allowed() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
        CliCommand::Sign {
            target,
            config_path,
        } => {
            let config = MurkletConfig::from_file(&config_path)?;
            let Some(signer) = config.request_signer()? else {
                return Err(format!(
                    "server.signing_secret is not configured in {}",
                    config_path.display()
                )
                .into());
            };
            println!("{}", signer.sign_load_url(&target));
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Serve { config_path } => {
            let config = MurkletConfig::from_file(&config_path)?;
            init_tracing(config.server.log_format);
            serve(config).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn serve(config: MurkletConfig) -> Result<(), Box<dyn std::error::Error>> {
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

    let s3_origin = config.s3_origin_config()?.map(S3Origin::new).transpose()?;

    let cache = FsCache::new(&config.cache.dir, config.cache.max_entry_bytes)?
        .with_max_total_bytes(config.cache.max_total_bytes)
        .with_ttl(config.cache.ttl_secs.map(Duration::from_secs));

    let source_cache = if config.cache.source.enabled {
        Some(
            FsCache::new(&config.cache.source.dir, config.limits.max_input_bytes)?
                .with_max_total_bytes(config.cache.source.max_total_bytes)
                .with_ttl(config.cache.source.ttl_secs.map(Duration::from_secs)),
        )
    } else {
        None
    };

    let needs_variant_sweep =
        config.cache.max_total_bytes.is_some() || config.cache.ttl_secs.is_some();
    let needs_source_sweep = source_cache.is_some()
        && (config.cache.source.max_total_bytes.is_some()
            || config.cache.source.ttl_secs.is_some());

    let sweeper = if needs_variant_sweep || needs_source_sweep {
        let variant_handle = needs_variant_sweep.then(|| cache.clone());
        let source_handle = needs_source_sweep.then(|| source_cache.clone()).flatten();
        let interval = Duration::from_secs(config.cache.sweep_interval_secs);
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Some(vc) = variant_handle.clone() {
                    run_eviction_sweep("variant", vc).await;
                }
                if let Some(sc) = source_handle.clone() {
                    run_eviction_sweep("source", sc).await;
                }
            }
        }))
    } else {
        None
    };

    let request_signer = config.request_signer()?;
    let presets = config.preset_registry()?;
    let origin_policies = config.origin_policies()?;
    let engine_limits = config.engine_limits();
    let watermarks = WatermarkRegistry::load_from_paths(&config.watermarks, &engine_limits)?;

    let mut state = AppState::from_parts(local_origin, http_origin, cache)
        .with_max_concurrent_transforms(config.limits.max_concurrent_transforms)
        .with_origin_policies(origin_policies);
    if let Some(s3) = s3_origin {
        state = state.with_s3_origin(s3);
    }
    if let Some(sc) = source_cache {
        state = state.with_source_cache(sc);
    }
    state.engine_limits = engine_limits;
    state.max_target_dimension = config.limits.max_target_dimension;
    state.transform_queue_timeout = Duration::from_secs(config.limits.transform_queue_timeout_secs);
    state.cache_control = config.server.cache_control;
    state.accept_negotiation = config.server.accept_negotiation;
    state.metrics_enabled = config.server.metrics_enabled;
    state.cleanup_token = config.server.cleanup_token;
    state.request_signer = request_signer;
    state.presets = presets;
    state.watermarks = watermarks;
    state.diagnostic_headers = config.server.diagnostic_headers;

    let app = build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    tracing::info!(
        bind = %config.server.bind,
        local_origin = config.origins.local.enabled,
        http_origin = config.origins.http.enabled,
        s3_origin = config.origins.s3.enabled,
        metrics_enabled = config.server.metrics_enabled,
        presets = config.presets.len(),
        watermarks = config.watermarks.len(),
        "murklet server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    if let Some(handle) = sweeper {
        handle.abort();
    }

    Ok(())
}

async fn run_eviction_sweep(label: &'static str, cache: FsCache) {
    match tokio::task::spawn_blocking(move || cache.evict()).await {
        Ok(Ok(report)) => {
            if report.expired_removed > 0 || report.evicted_for_size > 0 {
                tracing::info!(
                    cache = label,
                    expired_removed = report.expired_removed,
                    evicted_for_size = report.evicted_for_size,
                    freed_bytes = report.freed_bytes,
                    remaining_entries = report.remaining_entries,
                    remaining_bytes = report.remaining_bytes,
                    "cache eviction sweep completed"
                );
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(cache = label, error = %err, "cache eviction sweep failed");
        }
        Err(err) => {
            tracing::warn!(cache = label, error = %err, "cache eviction task panicked");
        }
    }
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

fn parse_cli_args() -> Result<CliCommand, String> {
    let mut args = env::args().skip(1).peekable();

    if args.peek().is_some_and(|arg| arg == "explain") {
        let _ = args.next();
        return parse_explain_args(args);
    }

    if args.peek().is_some_and(|arg| arg == "sign") {
        let _ = args.next();
        return parse_sign_args(args);
    }

    let mut check_only = false;
    let mut explicit_path: Option<PathBuf> = None;

    for arg in args {
        if arg == "--check" {
            check_only = true;
        } else if arg.starts_with('-') {
            return Err(format!(
                "unrecognised flag '{arg}' (supported: --check, explain <url> [config], sign <url> [config])"
            ));
        } else if explicit_path.is_none() {
            explicit_path = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
    }

    let config_path = resolve_config_path(explicit_path);
    if check_only {
        Ok(CliCommand::Check { config_path })
    } else {
        Ok(CliCommand::Serve { config_path })
    }
}

fn parse_explain_args(mut args: impl Iterator<Item = String>) -> Result<CliCommand, String> {
    let mut target: Option<String> = None;
    let mut explicit_path: Option<PathBuf> = None;
    let mut accept: Option<String> = None;

    while let Some(arg) = args.next() {
        if arg == "--accept" {
            let value = args
                .next()
                .ok_or_else(|| "flag '--accept' requires a header value".to_owned())?;
            accept = Some(value);
        } else if arg.starts_with('-') {
            return Err(format!(
                "unrecognised flag '{arg}' for explain (supported: --accept <value>)"
            ));
        } else if target.is_none() {
            target = Some(arg);
        } else if explicit_path.is_none() {
            explicit_path = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
    }

    let target = target.ok_or_else(|| {
        "usage: murklet explain [--accept <header>] <load-url-or-query> [config]".to_owned()
    })?;

    Ok(CliCommand::Explain {
        target,
        config_path: resolve_config_path(explicit_path),
        accept,
    })
}

fn parse_sign_args(args: impl Iterator<Item = String>) -> Result<CliCommand, String> {
    let mut target: Option<String> = None;
    let mut explicit_path: Option<PathBuf> = None;

    for arg in args {
        if arg.starts_with('-') {
            return Err(format!("unrecognised flag '{arg}' for sign"));
        } else if target.is_none() {
            target = Some(arg);
        } else if explicit_path.is_none() {
            explicit_path = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
    }

    let target =
        target.ok_or_else(|| "usage: murklet sign <load-url-or-query> [config]".to_owned())?;

    Ok(CliCommand::Sign {
        target,
        config_path: resolve_config_path(explicit_path),
    })
}

fn resolve_config_path(explicit_path: Option<PathBuf>) -> PathBuf {
    explicit_path
        .or_else(|| {
            env::var("MURKLET_CONFIG")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
