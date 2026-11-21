//! Axum router and HTTP handlers wiring domain, origins, engine, and cache layers.

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
use tokio::sync::Semaphore;

use crate::cache::{CacheKey, Coalescer, FsCache, compute_etag};
use crate::domain::{
    DEFAULT_MAX_DIMENSION, ImageDimensions, OutputFormat, SelectedFilter, TransformSpec,
};
use crate::engine::{self, EngineLimits};
use crate::http::error::HttpError;
use crate::http::metrics::{
    CacheOutcome, Metrics, OriginKind, PROMETHEUS_CONTENT_TYPE, StatusClass,
};
use crate::http::negotiate::negotiate_format;
use crate::http::query::{RawLoadQuery, parse_load_query_with_presets};
use crate::origin::{HttpOrigin, LocalFsOrigin};
pub use crate::policy::DEFAULT_CACHE_CONTROL;
use crate::policy::{DEFAULT_MAX_CONCURRENT_TRANSFORMS, PresetRegistry, RequestSigner};

const LOCAL_ORIGIN_ID: &str = "local";
const HTTP_ORIGIN_ID: &str = "http";

/// Shared output of a cache-miss fetch and transformation, holding refcounted
/// [`Bytes`] so coalesced followers share encoded payloads without copying.
#[derive(Debug)]
struct MissOutcome {
    bytes: Bytes,
    format: OutputFormat,
    source_dimensions: ImageDimensions,
    output_dimensions: ImageDimensions,
    filter: SelectedFilter,
    original_size: usize,
    etag: String,
}

type VariantCoalescer = Coalescer<CacheKey, Result<Arc<MissOutcome>, HttpError>>;

/// Shared immutable application state injected into HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Optional local filesystem origin adapter.
    pub local_origin: Option<LocalFsOrigin>,
    /// Optional HTTP(S) origin adapter (disabled by default unless configured with an allowlist).
    pub http_origin: Option<HttpOrigin>,
    /// Filesystem-backed variant cache.
    pub cache: FsCache,
    /// Resource limits and animation fallback policy for the image engine.
    pub engine_limits: EngineLimits,
    /// Maximum target width or height accepted in query parameters.
    pub max_target_dimension: u32,
    /// `Cache-Control` header value emitted on image responses.
    pub cache_control: String,
    /// Optional secret token required by `POST /cache/cleanup`.
    /// When `None` or empty, cleanup requests fail closed with `401 Unauthorized`.
    pub cleanup_token: Option<String>,
    /// Optional HMAC-SHA256 verifier requiring a valid `sig` parameter on `GET /load`.
    pub request_signer: Option<RequestSigner>,
    /// Validated named transformation presets (`[presets.<name>]`).
    pub presets: PresetRegistry,
    /// When `true`, negotiates output format from the `Accept` header when `f` is omitted.
    pub accept_negotiation: bool,
    /// When `true`, exposes Prometheus metrics at `GET /metrics`.
    pub metrics_enabled: bool,
    /// When `true`, emits extended `X-Original-*` / `X-Processed-*` diagnostic headers on cache MISS.
    pub diagnostic_headers: bool,
    coalescer: VariantCoalescer,
    transform_permits: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}

impl AppState {
    /// Constructs application state from explicit optional origins and a cache.
    #[must_use]
    pub fn from_parts(
        local_origin: Option<LocalFsOrigin>,
        http_origin: Option<HttpOrigin>,
        cache: FsCache,
    ) -> Self {
        Self {
            local_origin,
            http_origin,
            cache,
            engine_limits: EngineLimits::default(),
            max_target_dimension: DEFAULT_MAX_DIMENSION,
            cache_control: DEFAULT_CACHE_CONTROL.to_owned(),
            cleanup_token: None,
            request_signer: None,
            presets: PresetRegistry::empty(),
            accept_negotiation: false,
            metrics_enabled: true,
            diagnostic_headers: false,
            coalescer: Coalescer::new(),
            transform_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_TRANSFORMS)),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Sets the maximum number of concurrent CPU-bound image transformations.
    #[must_use]
    pub fn with_max_concurrent_transforms(mut self, max_concurrent: usize) -> Self {
        self.transform_permits = Arc::new(Semaphore::new(max_concurrent.max(1)));
        self
    }

    /// Constructs application state backed by a local filesystem origin and cache.
    ///
    /// Remote HTTP(S) fetching is disabled by default; attach an allowlisted
    /// [`HttpOrigin`] via [`Self::with_http_origin`] to enable it.
    #[must_use]
    pub fn new(local_origin: LocalFsOrigin, cache: FsCache) -> Self {
        Self::from_parts(Some(local_origin), None, cache)
    }

    /// Attaches an allowlisted [`HttpOrigin`] for remote `http://` and `https://` requests.
    #[must_use]
    pub fn with_http_origin(mut self, http_origin: HttpOrigin) -> Self {
        self.http_origin = Some(http_origin);
        self
    }
}

/// Builds the Murklet Axum HTTP router.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/metrics", get(metrics_handler))
        .route("/load", get(load_handler))
        .route("/cache/cleanup", post(cleanup_handler))
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

    let mut parsed =
        parse_load_query_with_presets(raw_query, state.max_target_dimension, &state.presets)?;
    let vary_accept = state.accept_negotiation && !parsed.explicit_format;
    if vary_accept {
        let accept = headers.get(ACCEPT).and_then(|value| value.to_str().ok());
        parsed.spec.format = negotiate_format(accept);
    }

    let (origin_id, origin_kind) = resolve_origin_route(state, &parsed.source)?;
    let cache_key = CacheKey::from_parts(origin_id, &parsed.source, &parsed.spec);
    let if_none_match = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Check cache on the blocking pool so disk I/O never stalls async worker threads.
    let cache_lookup = {
        let state = Arc::clone(state);
        let key = cache_key.clone();
        tokio::task::spawn_blocking(move || state.cache.get(&key))
            .await
            .map_err(|_| HttpError::Internal)?
    };

    if let Ok(Some(cached)) = cache_lookup {
        let response = if etag_matches(if_none_match.as_deref(), &cached.etag) {
            build_not_modified_response(&cached.etag, &state.cache_control, "HIT", vary_accept)
        } else {
            build_image_response(
                Bytes::from(cached.bytes),
                cached.format.mime_type(),
                &cached.etag,
                &state.cache_control,
                "HIT",
                vary_accept,
                None,
            )
        };
        record_load_success(
            state,
            origin_kind,
            cached.format,
            CacheOutcome::Hit,
            response.status().as_u16(),
            started,
        );
        return Ok(response);
    }

    // Cache miss: coalesce concurrent requests for the same canonical variant key.
    let coalesced = state
        .coalescer
        .execute(cache_key.clone(), || {
            let state = Arc::clone(state);
            let source = parsed.source.clone();
            let spec = parsed.spec;
            let key = cache_key.clone();
            async move {
                fetch_transform_and_cache(state, origin_id, origin_kind, source, spec, key).await
            }
        })
        .await;

    let outcome = coalesced.value?;
    let response = build_miss_response(
        state,
        &outcome,
        parsed.spec.quality.get(),
        if_none_match.as_deref(),
        coalesced.was_coalesced,
        vary_accept,
    );
    let cache_outcome = if coalesced.was_coalesced {
        CacheOutcome::Coalesced
    } else {
        CacheOutcome::Miss
    };
    record_load_success(
        state,
        origin_kind,
        outcome.format,
        cache_outcome,
        response.status().as_u16(),
        started,
    );
    Ok(response)
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

fn resolve_origin_route(
    state: &AppState,
    source: &str,
) -> Result<(&'static str, OriginKind), HttpError> {
    if is_http_source(source) {
        if state.http_origin.is_none() {
            return Err(HttpError::Forbidden(
                "remote HTTP(S) origins are disabled".to_owned(),
            ));
        }
        Ok((HTTP_ORIGIN_ID, OriginKind::Http))
    } else {
        if state.local_origin.is_none() {
            return Err(HttpError::Forbidden(
                "local filesystem origin is disabled".to_owned(),
            ));
        }
        Ok((LOCAL_ORIGIN_ID, OriginKind::Local))
    }
}

fn build_miss_response(
    state: &AppState,
    outcome: &MissOutcome,
    requested_quality: u8,
    if_none_match: Option<&str>,
    was_coalesced: bool,
    vary_accept: bool,
) -> Response {
    let x_cache = if was_coalesced { "HIT" } else { "MISS" };

    if etag_matches(if_none_match, &outcome.etag) {
        return build_not_modified_response(
            &outcome.etag,
            &state.cache_control,
            x_cache,
            vary_accept,
        );
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
        &state.cache_control,
        x_cache,
        vary_accept,
        diagnostics.as_ref(),
    )
}

async fn fetch_transform_and_cache(
    state: Arc<AppState>,
    origin_id: &'static str,
    origin_kind: OriginKind,
    source: String,
    spec: TransformSpec,
    key: CacheKey,
) -> Result<Arc<MissOutcome>, HttpError> {
    let started = Instant::now();
    let fetched = match origin_kind {
        OriginKind::Http => {
            let http_origin = state.http_origin.as_ref().ok_or_else(|| {
                HttpError::Forbidden("remote HTTP(S) origins are disabled".to_owned())
            })?;
            http_origin.fetch(&source).await?
        }
        OriginKind::Local => {
            let local_origin = state.local_origin.clone().ok_or_else(|| {
                HttpError::Forbidden("local filesystem origin is disabled".to_owned())
            })?;
            let source_for_fs = source.clone();
            tokio::task::spawn_blocking(move || local_origin.fetch(&source_for_fs))
                .await
                .map_err(|_| HttpError::Internal)??
        }
    };

    let original_size = fetched.bytes.len();

    // Bound concurrent CPU/memory-intensive image decoding and encoding.
    let _permit = Arc::clone(&state.transform_permits)
        .acquire_owned()
        .await
        .map_err(|_| HttpError::Internal)?;
    let _in_flight = state.metrics.track_in_flight();

    let metrics = Arc::clone(&state.metrics);
    tokio::task::spawn_blocking(move || -> Result<Arc<MissOutcome>, HttpError> {
        let output = engine::transform(&fetched.bytes, &spec, &state.engine_limits)?;

        // Store transformed bytes under a key matching the actual output format
        // (relevant if an animated source was passed through in its original format).
        let effective_key = if output.format == key.format() {
            key
        } else {
            let mut effective_spec = spec;
            effective_spec.format = output.format;
            CacheKey::from_parts(origin_id, &source, &effective_spec)
        };

        let etag = match state.cache.put(&effective_key, &output.bytes) {
            Ok(stored) => stored.etag,
            Err(_) => compute_etag(&output.bytes),
        };

        metrics.record_transform(origin_kind, output.format, started.elapsed());

        Ok(Arc::new(MissOutcome {
            bytes: Bytes::from(output.bytes),
            format: output.format,
            source_dimensions: output.source_dimensions,
            output_dimensions: output.output_dimensions,
            filter: output.filter,
            original_size,
            etag,
        }))
    })
    .await
    .map_err(|_| HttpError::Internal)?
}

fn is_http_source(source: &str) -> bool {
    source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || source
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

#[derive(Debug, Default, Deserialize)]
struct CleanupQuery {
    url: Option<String>,
}

async fn cleanup_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CleanupQuery>,
) -> Result<Response, HttpError> {
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

    if query.url.as_deref().is_some_and(|url| !url.is_empty()) {
        return Err(HttpError::NotImplemented(
            "per-URL cache cleanup is not yet implemented".to_owned(),
        ));
    }

    let cache = state.cache.clone();
    let removed = tokio::task::spawn_blocking(move || cache.cleanup())
        .await
        .map_err(|_| HttpError::Internal)??;

    state.metrics.record_cache_cleanup();
    tracing::info!(removed_files = removed, "cache cleanup completed");

    Ok((StatusCode::OK, "Cache cleaned successfully").into_response())
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

        // Per-URL cleanup returns 501.
        let per_url = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/cleanup?url=sample.png")
                    .header("x-cache-cleanup-token", "s3cr3t-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(per_url.status(), StatusCode::NOT_IMPLEMENTED);

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
}
