//! Axum HTTP transport layer: routing, authentication checks, conditional
//! responses (`ETag` / `If-None-Match`), diagnostic headers, and status mapping.
//!
//! Business orchestration (policy resolution, cache lookup, request coalescing,
//! resource isolation, origin fetching, transformation, and invalidation) lives
//! in [`crate::application::load`].

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde::Deserialize;
use subtle::ConstantTimeEq;

pub use crate::application::AppState;
use crate::application::{
    LoadExecution, MissOutcome, cleanup_all_caches, execute_load, purge_source_entries,
    resolve_and_validate_load,
};
use crate::domain::OutputFormat;
use crate::http::error::HttpError;
use crate::http::metrics::{CacheOutcome, OriginKind, PROMETHEUS_CONTENT_TYPE, StatusClass};
use crate::http::query::RawLoadQuery;
pub use crate::policy::DEFAULT_CACHE_CONTROL;

/// Builds the Murklet Axum HTTP router.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/metrics", get(metrics_handler))
        .route("/load", get(load_handler))
        .route("/cache/cleanup", post(cleanup_handler))
        .route("/cache/purge", post(purge_handler))
        .with_state(state)
}

async fn healthz_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ok",
    )
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    if !state.metrics_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }

    (
        StatusCode::OK,
        [(CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        state.metrics.render_prometheus(),
    )
        .into_response()
}

async fn load_handler(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    headers: HeaderMap,
    Query(raw_query): Query<RawLoadQuery>,
) -> Response {
    let started = Instant::now();
    match handle_load_inner(&state, &uri, &headers, raw_query, started).await {
        Ok(response) => response,
        Err(err) => {
            let response = err.into_response();
            let status = response.status().as_u16();
            state
                .metrics
                .record_load_request(CacheOutcome::None, StatusClass::from_status_code(status));
            tracing::warn!(
                status,
                duration_ms = started.elapsed().as_millis(),
                "load request failed"
            );
            response
        }
    }
}

async fn handle_load_inner(
    state: &Arc<AppState>,
    uri: &Uri,
    headers: &HeaderMap,
    raw_query: RawLoadQuery,
    started: Instant,
) -> Result<Response, HttpError> {
    if let Some(signer) = &state.request_signer
        && !signer.verify_raw_query(uri.query().unwrap_or(""))
    {
        return Err(HttpError::Unauthorized);
    }

    let accept = headers.get(ACCEPT).and_then(|value| value.to_str().ok());
    let resolved = resolve_and_validate_load(state, accept, raw_query)?;
    let if_none_match = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match execute_load(state, &resolved).await? {
        LoadExecution::Hit(cached) => {
            let response = if etag_matches(if_none_match.as_deref(), &cached.etag) {
                build_not_modified_response(
                    &cached.etag,
                    &resolved.cache_control,
                    "HIT",
                    resolved.vary_accept,
                )
            } else {
                build_image_response(
                    Bytes::from(cached.bytes),
                    cached.format.mime_type(),
                    &cached.etag,
                    &resolved.cache_control,
                    "HIT",
                    resolved.vary_accept,
                    None,
                )
            };
            record_load_success(
                state,
                resolved.origin_kind,
                cached.format,
                CacheOutcome::Hit,
                response.status().as_u16(),
                started,
            );
            Ok(response)
        }
        LoadExecution::Miss {
            outcome,
            was_coalesced,
        } => {
            let response = build_miss_response(
                state,
                &outcome,
                resolved.spec.quality.get(),
                &resolved.cache_control,
                if_none_match.as_deref(),
                was_coalesced,
                resolved.vary_accept,
            );
            let cache_outcome = if was_coalesced {
                CacheOutcome::Coalesced
            } else {
                CacheOutcome::Miss
            };
            record_load_success(
                state,
                resolved.origin_kind,
                outcome.format,
                cache_outcome,
                response.status().as_u16(),
                started,
            );
            Ok(response)
        }
    }
}

fn record_load_success(
    state: &AppState,
    origin: OriginKind,
    format: OutputFormat,
    cache: CacheOutcome,
    status: u16,
    started: Instant,
) {
    state
        .metrics
        .record_load_request(cache, StatusClass::from_status_code(status));
    tracing::info!(
        origin = origin.as_str(),
        format = format.as_str(),
        cache = ?cache,
        status,
        duration_ms = started.elapsed().as_millis(),
        "load request completed"
    );
}

#[derive(Debug, Default, Deserialize)]
struct CleanupQuery {
    url: Option<String>,
}

fn verify_cleanup_token(state: &AppState, headers: &HeaderMap) -> Result<(), HttpError> {
    let expected = state
        .cleanup_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or(HttpError::Unauthorized)?;

    let supplied = headers
        .get("x-cache-cleanup-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    if !constant_time_token_eq(supplied, expected) {
        return Err(HttpError::Unauthorized);
    }
    Ok(())
}

async fn purge_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CleanupQuery>,
) -> Result<Response, HttpError> {
    verify_cleanup_token(&state, &headers)?;
    let url = query
        .url
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::BadRequest("query parameter 'url' is required".to_owned()))?;

    let removed = purge_source_entries(&state, url).await?;
    Ok((
        StatusCode::OK,
        format!("Purged {removed} cached entries for source"),
    )
        .into_response())
}

async fn cleanup_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CleanupQuery>,
) -> Result<Response, HttpError> {
    verify_cleanup_token(&state, &headers)?;

    if let Some(url) = query.url.as_deref().filter(|url| !url.is_empty()) {
        let removed = purge_source_entries(&state, url).await?;
        return Ok((
            StatusCode::OK,
            format!("Purged {removed} cached entries for source"),
        )
            .into_response());
    }

    let _removed = cleanup_all_caches(&state).await?;
    Ok((StatusCode::OK, "Cache cleaned successfully").into_response())
}

fn build_miss_response(
    state: &AppState,
    outcome: &MissOutcome,
    requested_quality: u8,
    cache_control: &str,
    if_none_match: Option<&str>,
    was_coalesced: bool,
    vary_accept: bool,
) -> Response {
    let x_cache = if was_coalesced { "HIT" } else { "MISS" };

    if etag_matches(if_none_match, &outcome.etag) {
        return build_not_modified_response(&outcome.etag, cache_control, x_cache, vary_accept);
    }

    let diagnostics = (!was_coalesced && state.diagnostic_headers).then(|| DiagnosticMeta {
        original_width: outcome.source_dimensions.width(),
        original_height: outcome.source_dimensions.height(),
        processed_width: outcome.output_dimensions.width(),
        processed_height: outcome.output_dimensions.height(),
        original_size: outcome.original_size,
        processed_size: outcome.bytes.len(),
        format: outcome.format.as_str(),
        quality: requested_quality,
        filter: outcome.filter.to_string(),
    });

    build_image_response(
        outcome.bytes.clone(),
        outcome.format.mime_type(),
        &outcome.etag,
        cache_control,
        x_cache,
        vary_accept,
        diagnostics.as_ref(),
    )
}

struct DiagnosticMeta {
    original_width: u32,
    original_height: u32,
    processed_width: u32,
    processed_height: u32,
    original_size: usize,
    processed_size: usize,
    format: &'static str,
    quality: u8,
    filter: String,
}

fn build_not_modified_response(
    etag: &str,
    cache_control: &str,
    x_cache: &'static str,
    vary_accept: bool,
) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    let headers = response.headers_mut();
    insert_header(headers, ETAG, etag);
    insert_header(headers, CACHE_CONTROL, cache_control);
    insert_header(headers, HeaderName::from_static("x-cache"), x_cache);
    if vary_accept {
        insert_header(headers, VARY, "Accept");
    }
    response
}

fn build_image_response(
    bytes: Bytes,
    mime_type: &'static str,
    etag: &str,
    cache_control: &str,
    x_cache: &'static str,
    vary_accept: bool,
    diagnostics: Option<&DiagnosticMeta>,
) -> Response {
    let content_length = bytes.len().to_string();
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();

    insert_header(headers, CONTENT_TYPE, mime_type);
    insert_header(headers, CONTENT_LENGTH, &content_length);
    insert_header(headers, ETAG, etag);
    insert_header(headers, CACHE_CONTROL, cache_control);
    insert_header(headers, HeaderName::from_static("x-cache"), x_cache);
    if vary_accept {
        insert_header(headers, VARY, "Accept");
    }

    if let Some(meta) = diagnostics {
        insert_header(
            headers,
            HeaderName::from_static("x-original-width"),
            &meta.original_width.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-original-height"),
            &meta.original_height.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-processed-width"),
            &meta.processed_width.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-processed-height"),
            &meta.processed_height.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-original-size"),
            &meta.original_size.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-processed-size"),
            &meta.processed_size.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-size-reduction"),
            &format_size_reduction(meta.original_size, meta.processed_size),
        );
        insert_header(headers, HeaderName::from_static("x-format"), meta.format);
        insert_header(
            headers,
            HeaderName::from_static("x-quality"),
            &meta.quality.to_string(),
        );
        insert_header(
            headers,
            HeaderName::from_static("x-resize-filter"),
            &meta.filter,
        );
    }

    response
}

fn insert_header(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(header_value) = HeaderValue::from_str(value) {
        headers.insert(name, header_value);
    }
}

/// Checks whether an `If-None-Match` header matches the current strong `ETag`.
fn etag_matches(if_none_match: Option<&str>, current_etag: &str) -> bool {
    let Some(header) = if_none_match else {
        return false;
    };

    header.split(',').map(str::trim).any(|candidate| {
        if candidate == "*" {
            return true;
        }
        let normalised = candidate.strip_prefix("W/").unwrap_or(candidate);
        normalised == current_etag
    })
}

/// Compares two tokens in constant time.
fn constant_time_token_eq(supplied: &str, expected: &str) -> bool {
    let a = supplied.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    bool::from(a.ct_eq(b))
}

// Display-only percentage formatting; precision loss only occurs above 2^53 bytes.
#[allow(clippy::cast_precision_loss)]
fn format_size_reduction(original_size: usize, processed_size: usize) -> String {
    if original_size == 0 {
        return "0.00%".to_owned();
    }
    let orig = original_size as f64;
    let proc = processed_size as f64;
    let pct = ((orig - proc) / orig) * 100.0;
    format!("{pct:.2}%")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use image::{DynamicImage, ImageFormat, ImageReader, Rgba, RgbaImage};
    use tower::ServiceExt;

    use super::*;
    use crate::application::load::LOCAL_ORIGIN_ID;
    use crate::cache::{CacheKey, FsCache};
    use crate::domain::{DEFAULT_MAX_DIMENSION, TransformSpec};
    use crate::engine::WatermarkRegistry;
    use crate::origin::LocalFsOrigin;
    use crate::policy::{OriginPolicies, OriginPolicy, PresetRegistry, RequestSigner};
    use std::time::Duration;

    static NEXT_HTTP_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFixture {
        root: PathBuf,
        origin_dir: PathBuf,
        cache_dir: PathBuf,
    }

    impl TestFixture {
        fn new(label: &str) -> Self {
            let id = NEXT_HTTP_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let root = std::env::temp_dir().join(format!("murklet-http-{label}-{pid}-{id}"));
            let origin_dir = root.join("origin");
            let cache_dir = root.join("cache");
            fs::create_dir_all(&origin_dir).unwrap();
            fs::create_dir_all(&cache_dir).unwrap();

            // Write a deterministic 80x40 PNG fixture into the origin directory.
            let img = RgbaImage::from_fn(80, 40, |x, y| {
                let r = u8::try_from((x * 7 + y * 3) % 256).unwrap();
                let g = u8::try_from((x * 5 + y * 11) % 256).unwrap();
                let b = u8::try_from((x * 13 + y * 17) % 256).unwrap();
                Rgba([r, g, b, 255])
            });
            let mut png = Vec::new();
            DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                .unwrap();
            fs::write(origin_dir.join("sample.png"), png).unwrap();

            Self {
                root,
                origin_dir,
                cache_dir,
            }
        }

        fn state(&self) -> AppState {
            let origin = LocalFsOrigin::new(&self.origin_dir, 10 * 1024 * 1024).unwrap();
            let cache = FsCache::new(&self.cache_dir, 10 * 1024 * 1024).unwrap();
            AppState::new(origin, cache)
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let fixture = TestFixture::new("healthz");
        let app = build_router(Arc::new(fixture.state()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn load_transforms_caches_and_honours_if_none_match() {
        let fixture = TestFixture::new("load-flow");
        let app = build_router(Arc::new(fixture.state()));

        // First request: cache MISS, resized to 40x20 JPEG.
        let miss = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&q=80&f=jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(miss.status(), StatusCode::OK);
        assert_eq!(miss.headers().get("x-cache").unwrap(), "MISS");
        assert_eq!(miss.headers().get(CONTENT_TYPE).unwrap(), "image/jpeg");
        assert!(miss.headers().get(CACHE_CONTROL).is_some());
        assert!(miss.headers().get("x-original-width").is_none());

        let etag = miss
            .headers()
            .get(ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let miss_bytes = miss.into_body().collect().await.unwrap().to_bytes();

        let decoded = ImageReader::new(Cursor::new(&miss_bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 20));

        // Second request: cache HIT with identical ETag and bytes.
        let hit = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&q=80&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(hit.status(), StatusCode::OK);
        assert_eq!(hit.headers().get("x-cache").unwrap(), "HIT");
        assert_eq!(hit.headers().get(ETAG).unwrap(), etag.as_str());
        let hit_bytes = hit.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(hit_bytes, miss_bytes);

        // Third request with matching If-None-Match: 304 Not Modified and empty body.
        let not_modified = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&q=80&f=jpeg")
                    .header(IF_NONE_MATCH, etag.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers().get("x-cache").unwrap(), "HIT");
        let empty = not_modified.into_body().collect().await.unwrap().to_bytes();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn load_maps_validation_traversal_missing_and_disabled_remote_origins() {
        let fixture = TestFixture::new("errors");
        let app = build_router(Arc::new(fixture.state()));

        for (uri, expected_status) in [
            ("/load", StatusCode::BAD_REQUEST),
            ("/load?url=sample.png&w=abc", StatusCode::BAD_REQUEST),
            ("/load?url=../secret.png", StatusCode::BAD_REQUEST),
            ("/load?url=missing.png", StatusCode::NOT_FOUND),
            ("/load?url=https://example.com/a.png", StatusCode::FORBIDDEN),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                expected_status,
                "unexpected status for {uri}"
            );
        }
    }

    #[tokio::test]
    async fn load_emits_diagnostic_headers_when_enabled() {
        let fixture = TestFixture::new("diag");
        let mut state = fixture.state();
        state.diagnostic_headers = true;
        let app = build_router(Arc::new(state));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-original-width").unwrap(), "80");
        assert_eq!(response.headers().get("x-original-height").unwrap(), "40");
        assert_eq!(response.headers().get("x-processed-width").unwrap(), "40");
        assert_eq!(response.headers().get("x-processed-height").unwrap(), "20");
        assert_eq!(response.headers().get("x-format").unwrap(), "png");
    }

    #[tokio::test]
    async fn cleanup_enforces_token_method_and_purges_cache() {
        let fixture = TestFixture::new("cleanup");
        let mut state = fixture.state();
        state.cleanup_token = Some("s3cr3t-token".to_owned());
        let app = build_router(Arc::new(state));

        // Populate cache first.
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.headers().get("x-cache").unwrap(), "MISS");

        // GET is rejected with 405.
        let wrong_method = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/cache/cleanup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

        // Wrong token is rejected with 401.
        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/cleanup")
                    .header("x-cache-cleanup-token", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        // Valid full cleanup returns 200 and clears cached entries.
        let cleaned = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/cleanup")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cleaned.status(), StatusCode::OK);

        let after_cleanup = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(after_cleanup.headers().get("x-cache").unwrap(), "MISS");
    }

    #[tokio::test]
    async fn load_enforces_hmac_signature_when_request_signer_is_configured() {
        let fixture = TestFixture::new("signing");
        let signer = RequestSigner::new(b"test-signing-key".to_vec()).unwrap();
        let mut state = fixture.state();
        state.request_signer = Some(signer.clone());
        let app = build_router(Arc::new(state));

        // Unsigned request is rejected with 401.
        let unsigned = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);

        // Tampered request is rejected with 401.
        let sig = signer.sign_raw_query("url=sample.png&w=40");
        let tampered = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/load?url=sample.png&w=80&sig={sig}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tampered.status(), StatusCode::UNAUTHORIZED);

        // Valid signed request succeeds with 200.
        let valid_response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/load?url=sample.png&w=40&sig={sig}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn load_negotiates_format_via_accept_header_with_vary_and_separate_cache_keys() {
        let fixture = TestFixture::new("negotiate");
        let mut state = fixture.state();
        state.accept_negotiation = true;
        let app = build_router(Arc::new(state));

        // Omitted `f` with Accept: image/webp negotiates WebP and sets Vary: Accept.
        let webp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .header(ACCEPT, "image/webp,*/*;q=0.8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(webp.status(), StatusCode::OK);
        assert_eq!(webp.headers().get(CONTENT_TYPE).unwrap(), "image/webp");
        assert_eq!(webp.headers().get(VARY).unwrap(), "Accept");
        assert_eq!(webp.headers().get("x-cache").unwrap(), "MISS");

        // Omitted `f` with browser Accept including image/avif negotiates AVIF.
        let avif = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=16")
                    .header(ACCEPT, "image/avif,image/webp,*/*;q=0.8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(avif.status(), StatusCode::OK);
        assert_eq!(avif.headers().get(CONTENT_TYPE).unwrap(), "image/avif");
        assert_eq!(avif.headers().get(VARY).unwrap(), "Accept");

        // Same URL with Accept: image/jpeg gets a distinct JPEG variant (no cache poisoning).
        let jpeg = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .header(ACCEPT, "image/jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(jpeg.status(), StatusCode::OK);
        assert_eq!(jpeg.headers().get(CONTENT_TYPE).unwrap(), "image/jpeg");
        assert_eq!(jpeg.headers().get(VARY).unwrap(), "Accept");
        assert_eq!(jpeg.headers().get("x-cache").unwrap(), "MISS");

        // Explicit `f=png` bypasses Accept negotiation and omits Vary: Accept.
        let explicit_png = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=png")
                    .header(ACCEPT, "image/webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(explicit_png.status(), StatusCode::OK);
        assert_eq!(
            explicit_png.headers().get(CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert!(explicit_png.headers().get(VARY).is_none());
    }

    #[tokio::test]
    async fn metrics_endpoint_exposes_prometheus_counters_and_respects_toggle() {
        let fixture = TestFixture::new("metrics");
        let app = build_router(Arc::new(fixture.state()));

        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=32&f=webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let metrics_res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(metrics_res.status(), StatusCode::OK);
        assert_eq!(
            metrics_res.headers().get(CONTENT_TYPE).unwrap(),
            PROMETHEUS_CONTENT_TYPE
        );
        let body = String::from_utf8(
            metrics_res
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("murklet_load_requests_total{cache=\"miss\",status=\"200\"} 1"));
        assert!(body.contains("murklet_transforms_total{origin=\"local\",format=\"webp\"} 1"));

        // Disabled metrics endpoint returns 404.
        let mut disabled_state = fixture.state();
        disabled_state.metrics_enabled = false;
        let disabled_app = build_router(Arc::new(disabled_state));
        let not_found = disabled_app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn load_applies_named_preset_and_shares_cache_with_equivalent_explicit_query() {
        use std::collections::BTreeMap;

        use crate::policy::PresetConfig;

        let fixture = TestFixture::new("presets");
        let mut raw_presets = BTreeMap::new();
        raw_presets.insert(
            "thumb".to_owned(),
            PresetConfig {
                width: Some(40),
                height: None,
                quality: Some(80),
                format: Some("webp".to_owned()),
                filter: None,
                ..PresetConfig::default()
            },
        );

        let mut state = fixture.state();
        state.presets = PresetRegistry::from_config(&raw_presets, DEFAULT_MAX_DIMENSION).unwrap();
        let app = build_router(Arc::new(state));

        // Request via preset populates the cache.
        let via_preset = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&preset=thumb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_preset.status(), StatusCode::OK);
        assert_eq!(
            via_preset.headers().get(CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        assert_eq!(via_preset.headers().get("x-cache").unwrap(), "MISS");

        // Equivalent explicit query hits the exact same canonical cache entry.
        let via_explicit = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&q=80&f=webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_explicit.status(), StatusCode::OK);
        assert_eq!(via_explicit.headers().get("x-cache").unwrap(), "HIT");

        // Unknown preset returns 400 Bad Request.
        let unknown = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&preset=does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn saturated_transform_queue_returns_503_with_retry_after() {
        let fixture = TestFixture::new("backpressure");
        let mut state = fixture.state().with_max_concurrent_transforms(1);
        state.transform_queue_timeout = Duration::from_millis(25);

        // Hold the sole transform permit so incoming requests time out cleanly.
        let _held_permit = Arc::clone(&state.transform_permits)
            .acquire_owned()
            .await
            .unwrap();

        let app = build_router(Arc::new(state));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "2");
    }

    #[tokio::test]
    async fn load_composites_declared_watermark_and_rejects_unknown_name() {
        let fixture = TestFixture::new("watermarks");
        let mut watermarks = WatermarkRegistry::empty();
        watermarks
            .insert(
                "brand",
                image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255])),
            )
            .unwrap();

        let state = fixture.state().with_watermarks(watermarks);
        let app = build_router(Arc::new(state));

        let ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&f=png&wm=brand&wm_pos=center&wm_opacity=100&wm_scale=50")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let bytes = ok.into_body().collect().await.unwrap().to_bytes();
        let decoded = image::load_from_memory(&bytes).unwrap().into_rgb8();
        // Sample image is red/blue/etc., while centred 100% opaque white watermark makes centre white.
        let (cx, cy) = (decoded.width() / 2, decoded.height() / 2);
        assert_eq!(decoded.get_pixel(cx, cy).0, [255, 255, 255]);

        // Undeclared watermark is rejected with 400 Bad Request before touching origins/cache.
        let bad = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&wm=does_not_exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn load_enforces_per_origin_presets_only_format_and_cache_control_policy() {
        use std::collections::{BTreeMap, BTreeSet};

        use crate::policy::PresetConfig;

        let fixture = TestFixture::new("origin-policy");
        let mut raw_presets = BTreeMap::new();
        raw_presets.insert(
            "thumb".to_owned(),
            PresetConfig {
                width: Some(40),
                format: Some("webp".to_owned()),
                ..PresetConfig::default()
            },
        );
        let presets = PresetRegistry::from_config(&raw_presets, DEFAULT_MAX_DIMENSION).unwrap();

        let policies = OriginPolicies {
            local: OriginPolicy {
                presets_only: true,
                allowed_presets: BTreeSet::from(["thumb".to_owned()]),
                allowed_formats: vec![OutputFormat::Webp],
                max_target_dimension: Some(100),
                default_watermark: None,
                cache_control: Some("public, max-age=120".to_owned()),
            },
            ..OriginPolicies::default()
        };

        let mut state = fixture.state().with_origin_policies(policies);
        state.presets = presets;
        let app = build_router(Arc::new(state));

        // Allowed preset succeeds and emits per-origin Cache-Control.
        let ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&preset=thumb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers().get(CACHE_CONTROL).unwrap(),
            "public, max-age=120"
        );

        // Missing preset under presets_only returns 403 Forbidden.
        let no_preset = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(no_preset.status(), StatusCode::FORBIDDEN);

        // Ad-hoc override on top of preset under presets_only returns 403 Forbidden.
        let override_attempt = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&preset=thumb&q=50")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(override_attempt.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn purge_removes_single_source_variants_without_touching_others() {
        let fixture = TestFixture::new("selective-purge");
        std::fs::copy(
            fixture.origin_dir.join("sample.png"),
            fixture.origin_dir.join("keep.png"),
        )
        .unwrap();
        let mut state = fixture.state();
        state.cleanup_token = Some("s3cr3t-token".to_owned());
        let app = build_router(Arc::new(state));

        for uri in [
            "/load?url=sample.png&w=40&f=jpeg",
            "/load?url=sample.png&w=20&f=webp",
            "/load?url=keep.png&w=40&f=jpeg",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.headers().get("x-cache").unwrap(), "MISS");
        }

        // `/cache/purge` requires `url`.
        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/purge")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        // Purging `sample.png` removes both its JPEG and WebP variants while `keep.png` stays HIT.
        let purged = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/purge?url=sample.png")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(purged.status(), StatusCode::OK);

        let sample_jpg = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sample_jpg.headers().get("x-cache").unwrap(), "MISS");

        let keep_jpg = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=keep.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(keep_jpg.headers().get("x-cache").unwrap(), "HIT");

        // `/cache/cleanup?url=keep.png` performs the same selective purge.
        let via_cleanup = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/cleanup?url=keep.png")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_cleanup.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cache_read_and_write_errors_degrade_gracefully_and_increment_metrics() {
        let fixture = TestFixture::new("cache-errors");
        let local = LocalFsOrigin::new(&fixture.origin_dir, 1024 * 1024).unwrap();
        // Tiny max_entry_bytes forces both `get` (when an oversized file sits on disk) and
        // `put` (when the encoded variant exceeds 16 bytes) to return `CacheError::EntryTooLarge`.
        let cache = FsCache::new(&fixture.cache_dir, 16).unwrap();

        let spec = TransformSpec {
            dimensions: crate::domain::TargetDimensions::new(
                Some(crate::domain::Dimension::new(crate::domain::Axis::Width, 40, 8192).unwrap()),
                None,
            ),
            format: OutputFormat::Jpeg,
            ..TransformSpec::default()
        };
        let key = CacheKey::from_parts(LOCAL_ORIGIN_ID, "sample.png", &spec);
        let entry_dir = fixture
            .cache_dir
            .join(key.shard_dir())
            .join(key.source_digest_hex());
        fs::create_dir_all(&entry_dir).unwrap();
        fs::write(entry_dir.join(key.file_name()), vec![0u8; 64]).unwrap();

        let state = Arc::new(AppState::new(local, cache));
        let app = build_router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-cache").unwrap(), "MISS");

        let metrics_resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metrics_resp.status(), StatusCode::OK);
        let body = metrics_resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("murklet_cache_errors_total{cache=\"variant\",op=\"read\"} 1\n"));
        assert!(text.contains("murklet_cache_errors_total{cache=\"variant\",op=\"write\"} 1\n"));
    }

    #[tokio::test]
    async fn saturated_pending_fetch_and_byte_limits_return_503_with_retry_after() {
        let fixture = TestFixture::new("resource-isolation");

        // 1. Saturated pending queue fails fast before fetching.
        let mut pending_state = fixture.state().with_max_pending_transforms(1);
        pending_state.transform_queue_timeout = Duration::from_secs(5);
        let pending_state = Arc::new(pending_state);
        let held_pending = Arc::clone(&pending_state.pending_transform_permits)
            .acquire_owned()
            .await
            .unwrap();
        let pending_resp = build_router(Arc::clone(&pending_state))
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pending_resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(pending_resp.headers().get("retry-after").unwrap(), "2");
        assert!(
            pending_state
                .metrics
                .render_prometheus()
                .contains("murklet_backpressure_rejections_total{reason=\"pending_queue\"} 1\n")
        );
        drop(held_pending);

        // 2. Saturated fetch permits time out with 503 Retry-After.
        let mut fetch_state = fixture.state().with_max_concurrent_fetches(1);
        fetch_state.transform_queue_timeout = Duration::from_millis(30);
        let fetch_state = Arc::new(fetch_state);
        let held_fetch = Arc::clone(&fetch_state.fetch_permits)
            .acquire_owned()
            .await
            .unwrap();
        let fetch_resp = build_router(Arc::clone(&fetch_state))
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fetch_resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fetch_resp.headers().get("retry-after").unwrap(), "2");
        assert!(
            fetch_state
                .metrics
                .render_prometheus()
                .contains("murklet_backpressure_rejections_total{reason=\"fetch_timeout\"} 1\n")
        );
        drop(held_fetch);

        // 3. Saturated in-flight source byte budget times out with 503 Retry-After.
        let mut byte_state = fixture.state().with_max_in_flight_source_bytes(1024);
        byte_state.transform_queue_timeout = Duration::from_millis(30);
        let byte_state = Arc::new(byte_state);
        let _held_bytes = Arc::clone(&byte_state.in_flight_byte_permits)
            .acquire_many_owned(1)
            .await
            .unwrap();
        let byte_resp = build_router(Arc::clone(&byte_state))
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(byte_resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(byte_resp.headers().get("retry-after").unwrap(), "2");
        assert!(
            byte_state
                .metrics
                .render_prometheus()
                .contains("murklet_backpressure_rejections_total{reason=\"byte_budget\"} 1\n")
        );
    }

    #[tokio::test]
    async fn purge_during_in_flight_transform_prevents_stale_cache_write() {
        let fixture = TestFixture::new("purge-race");
        let mut state = fixture.state().with_max_concurrent_transforms(1);
        state.cleanup_token = Some("s3cr3t-token".to_owned());
        state.transform_queue_timeout = Duration::from_secs(5);
        let state = Arc::new(state);
        let app = build_router(Arc::clone(&state));

        // Hold the sole transform slot so the in-flight request fetches the old source
        // bytes and pauses before transforming and writing to cache.
        let held_transform = Arc::clone(&state.transform_permits)
            .acquire_owned()
            .await
            .unwrap();

        let in_flight_app = app.clone();
        let in_flight = tokio::spawn(async move {
            in_flight_app
                .oneshot(
                    Request::builder()
                        .uri("/load?url=sample.png&w=40&f=jpeg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        });

        // Wait briefly for the in-flight request to register its FlightToken and pause
        // on the transform semaphore.
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Replace the source image on disk with a visually distinct colour and purge.
        let updated = RgbaImage::from_pixel(100, 60, Rgba([220, 20, 40, 255]));
        let mut updated_bytes = Vec::new();
        DynamicImage::ImageRgba8(updated)
            .write_to(&mut Cursor::new(&mut updated_bytes), ImageFormat::Png)
            .unwrap();
        fs::write(fixture.origin_dir.join("sample.png"), updated_bytes).unwrap();

        let purge_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/purge?url=sample.png")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(purge_resp.status(), StatusCode::OK);

        // Release the transform slot so the stale in-flight request completes.
        drop(held_transform);
        let stale_resp = in_flight.await.unwrap();
        assert_eq!(stale_resp.status(), StatusCode::OK);
        let stale_etag = stale_resp
            .headers()
            .get(ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        // Next request must MISS (stale write was skipped) and serve the updated source.
        let fresh_resp = app
            .oneshot(
                Request::builder()
                    .uri("/load?url=sample.png&w=40&f=jpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fresh_resp.status(), StatusCode::OK);
        assert_eq!(fresh_resp.headers().get("x-cache").unwrap(), "MISS");
        let fresh_etag = fresh_resp.headers().get(ETAG).unwrap().to_str().unwrap();
        assert_ne!(fresh_etag, stale_etag);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_request_mid_transform_keeps_permits_held_until_blocking_work_finishes() {
        let fixture = TestFixture::new("cancel-transform");
        // Write a 160x120 image so debug-mode AVIF encoding takes ~0.5-1.5s of CPU time.
        let large = RgbaImage::from_fn(160, 120, |x, y| {
            let r = u8::try_from((x * 7 + y * 3) % 256).unwrap();
            let g = u8::try_from((x * 5 + y * 11) % 256).unwrap();
            let b = u8::try_from((x * 13 + y * 17) % 256).unwrap();
            Rgba([r, g, b, 255])
        });
        let mut large_bytes = Vec::new();
        DynamicImage::ImageRgba8(large)
            .write_to(&mut Cursor::new(&mut large_bytes), ImageFormat::Png)
            .unwrap();
        fs::write(fixture.origin_dir.join("heavy.png"), large_bytes).unwrap();

        let state = Arc::new(fixture.state().with_max_concurrent_transforms(1));
        let app = build_router(Arc::clone(&state));

        let req_task = tokio::spawn(async move {
            let _ = app
                .oneshot(
                    Request::builder()
                        .uri("/load?url=heavy.png&w=120&f=avif&q=80")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
        });

        // Wait until the request enters `spawn_blocking` and acquires the sole transform permit.
        let entered = tokio::time::timeout(Duration::from_secs(5), async {
            while state.transform_permits.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(
            entered.is_ok(),
            "transform task never acquired transform_permits"
        );

        // Simulate client disconnect by aborting the HTTP request task mid-encode.
        req_task.abort();
        let _ = req_task.await;

        // Immediately after cancellation, the blocking AVIF encode is still running and
        // MUST still hold the transform permit and in-flight gauge.
        assert_eq!(state.transform_permits.available_permits(), 0);
        assert!(
            state
                .metrics
                .render_prometheus()
                .contains("murklet_in_flight_transforms 1\n")
        );

        // Once the blocking encode finishes, permit and gauge return cleanly to idle.
        let released = tokio::time::timeout(Duration::from_secs(10), async {
            while state.transform_permits.available_permits() != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "transform permit was not released after blocking work finished"
        );
        assert!(
            state
                .metrics
                .render_prometheus()
                .contains("murklet_in_flight_transforms 0\n")
        );
    }
}
