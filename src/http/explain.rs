//! Policy and routing explanation engine powering `murklet explain <url>`.
//!
//! Evaluates a `/load` request against a [`MurkletConfig`] without starting the
//! server or performing network/filesystem fetches.

use std::fmt;

use url::form_urlencoded;

use crate::cache::CacheKey;
use crate::domain::TransformSpec;
use crate::http::negotiate::negotiate_format;
use crate::http::query::{RawLoadQuery, parse_load_query_with_presets};
use crate::origin::{HostAllowlist, validate_remote_url, validate_s3_key};
use crate::policy::MurkletConfig;

/// Result of explaining how Murklet would evaluate a `/load` request under a given config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainReport {
    /// Request passes all policy, signature, origin, and parameter checks.
    Allowed {
        /// Selected origin identifier (`"local"` or `"http"`).
        origin: &'static str,
        /// Optional matched preset name.
        preset: Option<String>,
        /// Resolved canonical transformation specification.
        spec: TransformSpec,
        /// Whether `Vary: Accept` would be emitted.
        vary_accept: bool,
        /// Resulting canonical [`CacheKey`].
        cache_key: CacheKey,
        /// Human-readable signature verification status.
        signature_status: &'static str,
        /// Summary of active limits.
        limits_summary: String,
    },
    /// Request would be rejected before fetching or transforming.
    Denied {
        /// Human-readable explanation of why the request was rejected.
        reason: String,
    },
}

impl ExplainReport {
    /// Reports whether the explained request is permitted.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

impl fmt::Display for ExplainReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed {
                origin,
                preset,
                spec,
                vary_accept,
                cache_key,
                signature_status,
                limits_summary,
            } => {
                writeln!(f, "Decision:  ALLOWED")?;
                writeln!(f, "Origin:    {origin}")?;
                writeln!(f, "Preset:    {}", preset.as_deref().unwrap_or("none"))?;
                writeln!(
                    f,
                    "Spec:      format={} quality={} width={} height={} fit={} gravity={} rotate={} filter={} bg={} filters={}",
                    spec.format,
                    spec.quality.get(),
                    spec.dimensions
                        .width()
                        .map_or_else(|| "auto".to_owned(), |d| d.get().to_string()),
                    spec.dimensions
                        .height()
                        .map_or_else(|| "auto".to_owned(), |d| d.get().to_string()),
                    spec.fit,
                    spec.gravity,
                    spec.rotation,
                    spec.filter
                        .map_or_else(|| "auto".to_owned(), |flt| flt.to_string()),
                    spec.background.map_or_else(
                        || "none".to_owned(),
                        crate::domain::BackgroundColour::to_hex
                    ),
                    spec.filters,
                )?;
                writeln!(f, "Canonical: {}", spec.canonical_representation())?;
                writeln!(
                    f,
                    "CacheKey:  {}/{} (vary_accept={vary_accept})",
                    cache_key.shard_dir(),
                    cache_key.file_name()
                )?;
                writeln!(f, "Signature: {signature_status}")?;
                write!(f, "Limits:    {limits_summary}")
            }
            Self::Denied { reason } => write!(f, "Decision:  DENIED\nReason:    {reason}"),
        }
    }
}

/// Evaluates a `/load` URL (or bare query string) against `config` and an optional
/// `Accept` header, returning a structured [`ExplainReport`].
#[must_use]
pub fn explain_load_request(
    config: &MurkletConfig,
    input: &str,
    accept_header: Option<&str>,
) -> ExplainReport {
    let raw_query = extract_query_string(input);

    let signature_status = match config.request_signer() {
        Ok(Some(signer)) => {
            if signer.verify_raw_query(raw_query) {
                "valid (HMAC-SHA256)"
            } else {
                return ExplainReport::Denied {
                    reason: "invalid or missing HMAC-SHA256 'sig' parameter".to_owned(),
                };
            }
        }
        Ok(None) => "not required",
        Err(err) => {
            return ExplainReport::Denied {
                reason: err.to_string(),
            };
        }
    };

    let presets = match config.preset_registry() {
        Ok(registry) => registry,
        Err(err) => {
            return ExplainReport::Denied {
                reason: err.to_string(),
            };
        }
    };

    let parsed_raw = decode_raw_query(raw_query);
    let mut parsed = match parse_load_query_with_presets(
        parsed_raw,
        config.limits.max_target_dimension,
        &presets,
    ) {
        Ok(parsed) => parsed,
        Err(err) => {
            return ExplainReport::Denied {
                reason: format!("{err:?}"),
            };
        }
    };

    let vary_accept = config.server.accept_negotiation && !parsed.explicit_format;
    if vary_accept {
        parsed.spec.format = negotiate_format(accept_header);
    }

    let origin = match classify_and_validate_origin(config, &parsed.source) {
        Ok(origin) => origin,
        Err(reason) => return ExplainReport::Denied { reason },
    };

    let cache_key = CacheKey::from_parts(origin, &parsed.source, &parsed.spec);
    let limits_summary = format!(
        "max_input={}B max_src_dim={} max_pixels={} max_concurrent={}",
        config.limits.max_input_bytes,
        config.limits.max_source_dimension,
        config.limits.max_decoded_pixels,
        config.limits.max_concurrent_transforms
    );

    ExplainReport::Allowed {
        origin,
        preset: parsed.preset,
        spec: parsed.spec,
        vary_accept,
        cache_key,
        signature_status,
        limits_summary,
    }
}

fn classify_and_validate_origin(
    config: &MurkletConfig,
    source: &str,
) -> Result<&'static str, String> {
    let is_remote = source
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || source
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"));
    let is_s3 = source
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("s3://"));

    if is_remote {
        if !config.origins.http.enabled {
            return Err("remote HTTP(S) origins are disabled".to_owned());
        }
        let allowlist = HostAllowlist::new(&config.origins.http.allowed_hosts);
        validate_remote_url(source, &allowlist, true).map_err(|err| err.to_string())?;
        Ok("http")
    } else if is_s3 {
        if !config.origins.s3.enabled {
            return Err("S3 origin is disabled".to_owned());
        }
        validate_s3_key(source).map_err(|err| err.to_string())?;
        Ok("s3")
    } else {
        if !config.origins.local.enabled {
            return Err("local filesystem origin is disabled".to_owned());
        }
        validate_local_path_lexically(source)?;
        Ok("local")
    }
}

fn extract_query_string(input: &str) -> &str {
    let trimmed = input.trim();
    if let Some((_, query)) = trimmed.split_once('?') {
        query
    } else {
        trimmed
    }
}

fn decode_raw_query(raw_query: &str) -> RawLoadQuery {
    let mut out = RawLoadQuery::default();
    for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "url" => out.url = Some(value.into_owned()),
            "preset" => out.preset = Some(value.into_owned()),
            "w" => out.w = Some(value.into_owned()),
            "h" => out.h = Some(value.into_owned()),
            "q" => out.q = Some(value.into_owned()),
            "f" => out.f = Some(value.into_owned()),
            "filter" => out.filter = Some(value.into_owned()),
            "fit" => out.fit = Some(value.into_owned()),
            "gravity" => out.gravity = Some(value.into_owned()),
            "rotate" => out.rotate = Some(value.into_owned()),
            "dpr" => out.dpr = Some(value.into_owned()),
            "bg" => out.bg = Some(value.into_owned()),
            "flip" => out.flip = Some(value.into_owned()),
            "grayscale" | "greyscale" => out.grayscale = Some(value.into_owned()),
            "invert" | "negative" => out.invert = Some(value.into_owned()),
            "blur" => out.blur = Some(value.into_owned()),
            "sharpen" => out.sharpen = Some(value.into_owned()),
            "brightness" => out.brightness = Some(value.into_owned()),
            "contrast" => out.contrast = Some(value.into_owned()),
            _ => {}
        }
    }
    out
}

fn validate_local_path_lexically(path: &str) -> Result<(), String> {
    if path.is_empty() || path.contains('\0') || path.contains('\\') || path.starts_with('/') {
        return Err("invalid local asset path".to_owned());
    }
    for segment in path.split('/') {
        if matches!(segment, "" | "." | "..") {
            return Err("local asset path contains forbidden segment".to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_allowed_preset_and_produces_same_cache_key_as_explicit_params() {
        let cfg = MurkletConfig::from_toml_str(
            r#"
[origins.http]
enabled = true
allowed_hosts = ["cdn.example.com"]

[presets.thumb]
width = 300
format = "webp"
quality = 80
"#,
        )
        .unwrap();

        let from_preset = explain_load_request(
            &cfg,
            "/load?url=https%3A%2F%2Fcdn.example.com%2Fhero.jpg&preset=thumb",
            None,
        );
        let from_explicit = explain_load_request(
            &cfg,
            "/load?url=https%3A%2F%2Fcdn.example.com%2Fhero.jpg&w=300&f=webp&q=80",
            None,
        );

        match (&from_preset, &from_explicit) {
            (
                ExplainReport::Allowed {
                    origin,
                    preset,
                    cache_key: preset_key,
                    ..
                },
                ExplainReport::Allowed {
                    cache_key: explicit_key,
                    ..
                },
            ) => {
                assert_eq!(*origin, "http");
                assert_eq!(preset.as_deref(), Some("thumb"));
                assert_eq!(preset_key, explicit_key);
            }
            _ => panic!("expected both explain reports to be Allowed"),
        }

        let rendered = from_preset.to_string();
        assert!(rendered.contains("Decision:  ALLOWED"));
        assert!(rendered.contains("Preset:    thumb"));
    }

    #[test]
    fn explains_denied_requests_for_disallowed_host_unknown_preset_and_traversal() {
        let cfg = MurkletConfig::from_toml_str(
            r#"
[origins.http]
enabled = true
allowed_hosts = ["cdn.example.com"]
"#,
        )
        .unwrap();

        assert!(
            !explain_load_request(&cfg, "/load?url=https://evil.example.com/a.jpg", None)
                .is_allowed()
        );
        assert!(
            !explain_load_request(&cfg, "/load?url=sample.png&preset=missing", None).is_allowed()
        );
        assert!(!explain_load_request(&cfg, "/load?url=../secret.png", None).is_allowed());
    }
}
