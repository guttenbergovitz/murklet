//! Axum router and HTTP handlers wiring domain, origins, engine, and cache layers.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::cache::{CacheKey, Coalescer, FsCache, compute_etag};
use crate::domain::{DEFAULT_MAX_DIMENSION, TransformSpec};
use crate::engine::{self, EngineLimits, TransformOutput};
use crate::http::error::HttpError;
use crate::http::query::{RawLoadQuery, parse_load_query};
use crate::origin::{HttpOrigin, LocalFsOrigin};

/// Default `Cache-Control` header applied to served image variants.
pub const DEFAULT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

const LOCAL_ORIGIN_ID: &str = "local";
const HTTP_ORIGIN_ID: &str = "http";

/// Shared output of a cache-miss fetch and transformation, wrapped in `Arc` so
/// coalesced followers share encoded bytes without redundant copying.
#[derive(Debug)]
struct MissOutcome {
    output: TransformOutput,
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
    /// When `true`, emits extended `X-Original-*` / `X-Processed-*` diagnostic headers on cache MISS.
    pub diagnostic_headers: bool,
    coalescer: VariantCoalescer,
}

impl AppState {
    /// Constructs application state backed by a local filesystem origin and cache.
    ///
    /// Remote HTTP(S) fetching is disabled by default; attach an allowlisted
    /// [`HttpOrigin`] via [`Self::with_http_origin`] to enable it.
    #[must_use]
    pub fn new(local_origin: LocalFsOrigin, cache: FsCache) -> Self {
        Self {
            local_origin: Some(local_origin),
            http_origin: None,
            cache,
            engine_limits: EngineLimits::default(),
            max_target_dimension: DEFAULT_MAX_DIMENSION,
            cache_control: DEFAULT_CACHE_CONTROL.to_owned(),
            cleanup_token: None,
            diagnostic_headers: false,
            coalescer: Coalescer::new(),
        }
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

async fn load_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(raw_query): Query<RawLoadQuery>,
) -> Result<Response, HttpError> {
    let parsed = parse_load_query(raw_query, state.max_target_dimension)?;
    let is_remote = is_http_source(&parsed.source);
    let origin_id = if is_remote {
        if state.http_origin.is_none() {
            return Err(HttpError::Forbidden(
                "remote HTTP(S) origins are disabled".to_owned(),
            ));
        }
        HTTP_ORIGIN_ID
    } else {
        if state.local_origin.is_none() {
            return Err(HttpError::Forbidden(
                "local filesystem origin is disabled".to_owned(),
            ));
        }
        LOCAL_ORIGIN_ID
    };

    let cache_key = CacheKey::from_parts(origin_id, &parsed.source, &parsed.spec);
    let if_none_match = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Check cache on the blocking pool so disk I/O never stalls async worker threads.
    let cache_lookup = {
        let state = Arc::clone(&state);
        let key = cache_key.clone();
        tokio::task::spawn_blocking(move || state.cache.get(&key))
            .await
            .map_err(|_| HttpError::Internal)?
    };

    if let Ok(Some(cached)) = cache_lookup {
        if etag_matches(if_none_match.as_deref(), &cached.etag) {
            return Ok(build_not_modified_response(
                &cached.etag,
                &state.cache_control,
                "HIT",
            ));
        }

        return Ok(build_image_response(
            cached.bytes,
            cached.format.mime_type(),
            &cached.etag,
            &state.cache_control,
            "HIT",
            None,
        ));
    }

    // Cache miss: coalesce concurrent requests for the same canonical variant key.
    let coalesced = state
        .coalescer
        .execute(cache_key.clone(), || {
            let state = Arc::clone(&state);
            let source = parsed.source.clone();
            let spec = parsed.spec;
            let key = cache_key.clone();
            async move {
                fetch_transform_and_cache(state, origin_id, is_remote, source, spec, key).await
            }
        })
        .await;

    let outcome = coalesced.value?;
    let x_cache = if coalesced.was_coalesced {
        "HIT"
    } else {
        "MISS"
    };

    if etag_matches(if_none_match.as_deref(), &outcome.etag) {
        return Ok(build_not_modified_response(
            &outcome.etag,
            &state.cache_control,
            x_cache,
        ));
    }

    let diagnostics =
        (!coalesced.was_coalesced && state.diagnostic_headers).then(|| DiagnosticMeta {
            original_width: outcome.output.source_dimensions.width(),
            original_height: outcome.output.source_dimensions.height(),
            processed_width: outcome.output.output_dimensions.width(),
            processed_height: outcome.output.output_dimensions.height(),
            original_size: outcome.original_size,
            processed_size: outcome.output.bytes.len(),
            format: outcome.output.format.as_str(),
            quality: parsed.spec.quality.get(),
            filter: outcome.output.filter.to_string(),
        });

    Ok(build_image_response(
        outcome.output.bytes.clone(),
        outcome.output.format.mime_type(),
        &outcome.etag,
        &state.cache_control,
        x_cache,
        diagnostics.as_ref(),
    ))
}

async fn fetch_transform_and_cache(
    state: Arc<AppState>,
    origin_id: &'static str,
    is_remote: bool,
    source: String,
    spec: TransformSpec,
    key: CacheKey,
) -> Result<Arc<MissOutcome>, HttpError> {
    let fetched = if is_remote {
        let http_origin = state.http_origin.as_ref().ok_or_else(|| {
            HttpError::Forbidden("remote HTTP(S) origins are disabled".to_owned())
        })?;
        http_origin.fetch(&source).await?
    } else {
        let local_origin = state.local_origin.clone().ok_or_else(|| {
            HttpError::Forbidden("local filesystem origin is disabled".to_owned())
        })?;
        let source_for_fs = source.clone();
        tokio::task::spawn_blocking(move || local_origin.fetch(&source_for_fs))
            .await
            .map_err(|_| HttpError::Internal)??
    };

    let original_size = fetched.bytes.len();

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

        Ok(Arc::new(MissOutcome {
            output,
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
    tokio::task::spawn_blocking(move || cache.cleanup())
        .await
        .map_err(|_| HttpError::Internal)??;

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

fn build_not_modified_response(etag: &str, cache_control: &str, x_cache: &'static str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    let headers = response.headers_mut();
    insert_header(headers, ETAG, etag);
    insert_header(headers, CACHE_CONTROL, cache_control);
    insert_header(headers, HeaderName::from_static("x-cache"), x_cache);
    response
}

fn build_image_response(
    bytes: Vec<u8>,
    mime_type: &'static str,
    etag: &str,
    cache_control: &str,
    x_cache: &'static str,
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
            (
                "/load?url=sample.png&f=avif",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
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
}
