//! Load, fetch, transform, cache, and invalidation orchestration.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Semaphore;

use crate::cache::{
    CacheKey, CachedVariant, Coalescer, FlightToken, FsCache, InvalidationTracker, compute_etag,
    source_digest_hex,
};
use crate::domain::{
    DEFAULT_MAX_DIMENSION, ImageDimensions, OutputFormat, SelectedFilter, TransformSpec,
};
use crate::engine::{self, EngineLimits, WatermarkRegistry};
use crate::http::error::HttpError;
use crate::http::metrics::{BackpressureReason, CacheOp, CacheTier, Metrics, OriginKind};
use crate::http::negotiate::negotiate_format_allowed;
use crate::http::query::{RawLoadQuery, parse_load_query_with_presets};
use crate::origin::{
    ByteBudget, ByteReservation, HttpOrigin, LocalFsOrigin, OriginError, S3Origin,
};
use crate::policy::{
    DEFAULT_CACHE_CONTROL, DEFAULT_MAX_CONCURRENT_FETCHES, DEFAULT_MAX_CONCURRENT_TRANSFORMS,
    DEFAULT_MAX_IN_FLIGHT_SOURCE_BYTES, DEFAULT_MAX_PENDING_TRANSFORMS,
    DEFAULT_TRANSFORM_QUEUE_TIMEOUT_SECS, OriginPolicies, OriginPolicy, PresetRegistry,
    RequestSigner,
};

pub(crate) const LOCAL_ORIGIN_ID: &str = "local";
const HTTP_ORIGIN_ID: &str = "http";
const S3_ORIGIN_ID: &str = "s3";

/// Raw source bytes paired with their RAII [`ByteReservation`] so memory permits
/// and `murklet_in_flight_source_bytes` stay accounted for from pre-allocation
/// until the last coalesced consumer finishes transforming.
#[derive(Debug)]
struct ReservedSource {
    bytes: Bytes,
    _reservation: ByteReservation,
}

/// Shared output of a cache-miss fetch and transformation, holding refcounted
/// [`Bytes`] so coalesced followers share encoded payloads without copying.
#[derive(Debug)]
pub struct MissOutcome {
    /// Encoded output image payload.
    pub bytes: Bytes,
    /// Actual output format produced by the engine.
    pub format: OutputFormat,
    /// Original source dimensions before transformation.
    pub source_dimensions: ImageDimensions,
    /// Final output dimensions after transformation.
    pub output_dimensions: ImageDimensions,
    /// Resampling filter selected for the transformation.
    pub filter: SelectedFilter,
    /// Original raw source payload size in bytes.
    pub original_size: usize,
    /// Strong quoted `ETag` computed from `bytes`.
    pub etag: String,
}

/// Result of executing a validated load request against cache, coalescer, origins, and engine.
#[derive(Debug)]
pub enum LoadExecution {
    /// Served directly from the variant cache.
    Hit(CachedVariant),
    /// Executed or coalesced onto an in-flight origin fetch and transformation.
    Miss {
        /// Shared encoded payload and transformation metadata.
        outcome: Arc<MissOutcome>,
        /// `true` when this caller shared an in-flight leader's output.
        was_coalesced: bool,
    },
}

/// Validated load parameters resolved against origin routing, per-origin policies,
/// named presets, watermarks, and `Accept` format negotiation.
#[derive(Debug)]
pub struct ResolvedLoad {
    /// Canonical origin identifier (`"local"`, `"http"`, or `"s3"`).
    pub origin_id: &'static str,
    /// Low-cardinality origin kind for metrics and logs.
    pub origin_kind: OriginKind,
    /// Source asset path or URL.
    pub source: String,
    /// Canonical transformation specification.
    pub spec: TransformSpec,
    /// `true` when output format was negotiated from the HTTP `Accept` header.
    pub vary_accept: bool,
    /// Effective `Cache-Control` header value for this origin.
    pub cache_control: String,
}

type VariantCoalescer = Coalescer<CacheKey, Result<Arc<MissOutcome>, HttpError>>;
type SourceCoalescer = Coalescer<String, Result<Arc<ReservedSource>, HttpError>>;

/// Shared immutable application state injected into HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Optional local filesystem origin adapter.
    pub local_origin: Option<LocalFsOrigin>,
    /// Optional HTTP(S) origin adapter (disabled by default unless configured with an allowlist).
    pub http_origin: Option<HttpOrigin>,
    /// Optional S3-compatible object storage origin adapter.
    pub s3_origin: Option<S3Origin>,
    /// Filesystem-backed variant cache.
    pub cache: FsCache,
    /// Optional filesystem-backed raw-source cache for remote HTTP(S) and S3 origins.
    pub source_cache: Option<FsCache>,
    /// Resource limits and animation fallback policy for the image engine.
    pub engine_limits: EngineLimits,
    /// Maximum target width or height accepted in query parameters.
    pub max_target_dimension: u32,
    /// Maximum duration a request waits for a transform concurrency slot before returning `503`.
    pub transform_queue_timeout: Duration,
    /// `Cache-Control` header value emitted on image responses.
    pub cache_control: String,
    /// Optional secret token required by `POST /cache/cleanup` and `POST /cache/purge`.
    /// When `None` or empty, cleanup requests fail closed with `401 Unauthorized`.
    pub cleanup_token: Option<String>,
    /// Optional HMAC-SHA256 verifier requiring a valid `sig` parameter on `GET /load`.
    pub request_signer: Option<RequestSigner>,
    /// Validated named transformation presets (`[presets.<name>]`).
    pub presets: PresetRegistry,
    /// Pre-decoded policy-controlled watermarks (`[watermarks]`).
    pub watermarks: WatermarkRegistry,
    /// Validated per-origin transformation and delivery policies (`[origins.<kind>.policy]`).
    pub origin_policies: OriginPolicies,
    /// When `true`, negotiates output format from the `Accept` header when `f` is omitted.
    pub accept_negotiation: bool,
    /// When `true`, exposes Prometheus metrics at `GET /metrics`.
    pub metrics_enabled: bool,
    /// When `true`, emits extended `X-Original-*` / `X-Processed-*` diagnostic headers on cache MISS.
    pub diagnostic_headers: bool,
    coalescer: VariantCoalescer,
    source_coalescer: SourceCoalescer,
    invalidation: InvalidationTracker,
    pub(crate) transform_permits: Arc<Semaphore>,
    pub(crate) fetch_permits: Arc<Semaphore>,
    pub(crate) pending_transform_permits: Arc<Semaphore>,
    pub(crate) in_flight_byte_permits: Arc<Semaphore>,
    source_byte_budget: ByteBudget,
    pub(crate) metrics: Arc<Metrics>,
}

impl AppState {
    /// Constructs application state from explicit optional origins and a cache.
    #[must_use]
    pub fn from_parts(
        local_origin: Option<LocalFsOrigin>,
        http_origin: Option<HttpOrigin>,
        cache: FsCache,
    ) -> Self {
        let metrics = Arc::new(Metrics::new());
        let source_byte_budget = ByteBudget::new(
            DEFAULT_MAX_IN_FLIGHT_SOURCE_BYTES,
            metrics.in_flight_source_bytes_counter(),
        );
        let in_flight_byte_permits = source_byte_budget.semaphore();
        Self {
            local_origin,
            http_origin,
            s3_origin: None,
            cache,
            source_cache: None,
            engine_limits: EngineLimits::default(),
            max_target_dimension: DEFAULT_MAX_DIMENSION,
            transform_queue_timeout: Duration::from_secs(DEFAULT_TRANSFORM_QUEUE_TIMEOUT_SECS),
            cache_control: DEFAULT_CACHE_CONTROL.to_owned(),
            cleanup_token: None,
            request_signer: None,
            presets: PresetRegistry::empty(),
            watermarks: WatermarkRegistry::empty(),
            origin_policies: OriginPolicies::default(),
            accept_negotiation: false,
            metrics_enabled: true,
            diagnostic_headers: false,
            coalescer: Coalescer::new(),
            source_coalescer: Coalescer::new(),
            invalidation: InvalidationTracker::new(),
            transform_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_TRANSFORMS)),
            fetch_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_FETCHES)),
            pending_transform_permits: Arc::new(Semaphore::new(DEFAULT_MAX_PENDING_TRANSFORMS)),
            in_flight_byte_permits,
            source_byte_budget,
            metrics,
        }
    }

    /// Attaches an [`S3Origin`] for `s3://` asset requests.
    #[must_use]
    pub fn with_s3_origin(mut self, s3_origin: S3Origin) -> Self {
        self.s3_origin = Some(s3_origin);
        self
    }

    /// Attaches an optional raw-source [`FsCache`] for remote HTTP(S) and S3 origins.
    #[must_use]
    pub fn with_source_cache(mut self, source_cache: FsCache) -> Self {
        self.source_cache = Some(source_cache);
        self
    }

    /// Attaches pre-decoded policy-controlled watermarks (`[watermarks]`).
    #[must_use]
    pub fn with_watermarks(mut self, watermarks: WatermarkRegistry) -> Self {
        self.watermarks = watermarks;
        self
    }

    /// Attaches validated per-origin policies (`[origins.<kind>.policy]`).
    #[must_use]
    pub fn with_origin_policies(mut self, origin_policies: OriginPolicies) -> Self {
        self.origin_policies = origin_policies;
        self
    }

    /// Sets the maximum number of concurrent CPU-bound image transformations.
    #[must_use]
    pub fn with_max_concurrent_transforms(mut self, max_concurrent: usize) -> Self {
        self.transform_permits = Arc::new(Semaphore::new(max_concurrent.max(1)));
        self
    }

    /// Sets the maximum number of concurrent origin fetches across all origin adapters.
    #[must_use]
    pub fn with_max_concurrent_fetches(mut self, max_fetches: usize) -> Self {
        self.fetch_permits = Arc::new(Semaphore::new(max_fetches.max(1)));
        self
    }

    /// Sets the maximum total in-flight cache-miss requests (active plus queued).
    #[must_use]
    pub fn with_max_pending_transforms(mut self, max_pending: usize) -> Self {
        self.pending_transform_permits = Arc::new(Semaphore::new(max_pending.max(1)));
        self
    }

    /// Sets the aggregate raw source byte budget held in memory across in-flight requests.
    #[must_use]
    pub fn with_max_in_flight_source_bytes(mut self, max_bytes: usize) -> Self {
        let budget = ByteBudget::new(max_bytes, self.metrics.in_flight_source_bytes_counter());
        self.in_flight_byte_permits = budget.semaphore();
        self.source_byte_budget = budget;
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

/// Validates a raw load query against origin routing, per-origin policies, named
/// presets, watermarks, and optional `Accept` format negotiation.
///
/// # Errors
///
/// Returns [`HttpError::BadRequest`] on invalid query parameters or unknown watermarks,
/// or [`HttpError::Forbidden`] when blocked by origin routing or per-origin policy.
pub fn resolve_and_validate_load(
    state: &AppState,
    accept_header: Option<&str>,
    raw_query: RawLoadQuery,
) -> Result<ResolvedLoad, HttpError> {
    let source_preview = raw_query
        .url
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::BadRequest("query parameter 'url' is required".to_owned()))?;
    let (origin_id, origin_kind) = resolve_origin_route(state, source_preview)?;
    let origin_policy = state.origin_policies.for_origin_id(origin_id);

    enforce_origin_query_policy(&raw_query, origin_policy)?;

    let effective_max_dim = origin_policy.effective_max_dimension(state.max_target_dimension);
    let mut parsed = parse_load_query_with_presets(raw_query, effective_max_dim, &state.presets)?;
    let vary_accept = state.accept_negotiation && !parsed.explicit_format;
    if vary_accept {
        parsed.spec.format =
            negotiate_format_allowed(accept_header, &origin_policy.allowed_formats);
    }
    if !origin_policy.is_format_allowed(parsed.spec.format) {
        return Err(HttpError::Forbidden(format!(
            "output format '{}' is not permitted for origin '{origin_id}'",
            parsed.spec.format
        )));
    }
    if parsed.spec.watermark.is_none() {
        parsed.spec.watermark = origin_policy.default_watermark_spec();
    }
    if let Some(ref wm) = parsed.spec.watermark
        && !state.watermarks.contains(&wm.name)
    {
        return Err(HttpError::BadRequest(format!(
            "unknown watermark '{}'",
            wm.name
        )));
    }

    Ok(ResolvedLoad {
        origin_id,
        origin_kind,
        source: parsed.source,
        spec: parsed.spec,
        vary_accept,
        cache_control: origin_policy
            .effective_cache_control(&state.cache_control)
            .to_owned(),
    })
}

/// Executes a validated load request against the variant cache, request coalescer,
/// origin adapters, resource-isolation guards, and image transformation engine.
///
/// # Errors
///
/// Propagates origin, engine, backpressure (`503`), or internal task errors as [`HttpError`].
pub async fn execute_load(
    state: &Arc<AppState>,
    resolved: &ResolvedLoad,
) -> Result<LoadExecution, HttpError> {
    let cache_key = CacheKey::from_parts(resolved.origin_id, &resolved.source, &resolved.spec);

    // Check cache on the blocking pool so disk I/O never stalls async worker threads.
    if let Some(cached) = lookup_variant_cache(state, &cache_key).await? {
        return Ok(LoadExecution::Hit(cached));
    }

    // Cache miss: coalesce concurrent requests for the same canonical variant key.
    let coalesced = state
        .coalescer
        .execute(cache_key.clone(), || {
            let state = Arc::clone(state);
            let source = resolved.source.clone();
            let spec = resolved.spec.clone();
            let key = cache_key.clone();
            let origin_id = resolved.origin_id;
            let origin_kind = resolved.origin_kind;
            async move {
                fetch_transform_and_cache(state, origin_id, origin_kind, source, spec, key).await
            }
        })
        .await;

    let outcome = coalesced.value?;
    Ok(LoadExecution::Miss {
        outcome,
        was_coalesced: coalesced.was_coalesced,
    })
}

/// Evicts all cached variants and any cached raw source entry belonging to `source_url`
/// under linearised invalidation epochs.
///
/// # Errors
///
/// Returns [`HttpError::Internal`] if the blocking task or underlying filesystem purge fails.
pub async fn purge_source_entries(
    state: &Arc<AppState>,
    source_url: &str,
) -> Result<u64, HttpError> {
    let origin_id = if is_http_source(source_url) {
        HTTP_ORIGIN_ID
    } else if is_s3_source(source_url) {
        S3_ORIGIN_ID
    } else {
        LOCAL_ORIGIN_ID
    };
    let digest = source_digest_hex(origin_id, source_url);

    let cache = state.cache.clone();
    let source_cache = state.source_cache.clone();
    let invalidation = state.invalidation.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<u64, crate::cache::CacheError> {
        invalidation.run_source_purge(&digest, || {
            let mut count = cache.purge_source(&digest)?;
            if let Some(src_cache) = source_cache
                && src_cache.delete_raw(&digest)?
            {
                count += 1;
            }
            Ok(count)
        })
    })
    .await
    .map_err(|_| HttpError::Internal)??;

    state.metrics.record_cache_cleanup();
    tracing::info!(
        origin = origin_id,
        removed_files = removed,
        "selective cache purge completed"
    );

    Ok(removed)
}

/// Evicts all entries from the variant cache and optional raw-source cache under
/// linearised invalidation epochs.
///
/// # Errors
///
/// Returns [`HttpError::Internal`] if the blocking task or underlying filesystem cleanup fails.
pub async fn cleanup_all_caches(state: &Arc<AppState>) -> Result<u64, HttpError> {
    let cache = state.cache.clone();
    let source_cache = state.source_cache.clone();
    let invalidation = state.invalidation.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<u64, crate::cache::CacheError> {
        invalidation.run_full_cleanup(|| {
            let mut count = cache.cleanup()?;
            if let Some(src_cache) = source_cache {
                count += src_cache.cleanup()?;
            }
            Ok(count)
        })
    })
    .await
    .map_err(|_| HttpError::Internal)??;

    state.metrics.record_cache_cleanup();
    tracing::info!(removed_files = removed, "cache cleanup completed");

    Ok(removed)
}

async fn lookup_variant_cache(
    state: &Arc<AppState>,
    cache_key: &CacheKey,
) -> Result<Option<CachedVariant>, HttpError> {
    let state_for_get = Arc::clone(state);
    let key = cache_key.clone();
    match tokio::task::spawn_blocking(move || state_for_get.cache.get(&key))
        .await
        .map_err(|_| HttpError::Internal)?
    {
        Ok(cached) => Ok(cached),
        Err(err) => {
            state
                .metrics
                .record_cache_error(CacheTier::Variant, CacheOp::Read);
            tracing::warn!(
                cache = "variant",
                op = "read",
                error = %err,
                "variant cache read failed; falling back to origin"
            );
            Ok(None)
        }
    }
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
    } else if is_s3_source(source) {
        if state.s3_origin.is_none() {
            return Err(HttpError::Forbidden("S3 origin is disabled".to_owned()));
        }
        Ok((S3_ORIGIN_ID, OriginKind::S3))
    } else {
        if state.local_origin.is_none() {
            return Err(HttpError::Forbidden(
                "local filesystem origin is disabled".to_owned(),
            ));
        }
        Ok((LOCAL_ORIGIN_ID, OriginKind::Local))
    }
}

fn enforce_origin_query_policy(
    raw_query: &RawLoadQuery,
    policy: &OriginPolicy,
) -> Result<(), HttpError> {
    let preset_name = raw_query.preset.as_deref().filter(|v| !v.is_empty());

    if policy.presets_only {
        if preset_name.is_none() {
            return Err(HttpError::Forbidden(
                "origin policy requires a named 'preset' parameter (presets_only)".to_owned(),
            ));
        }
        if raw_query.has_transform_overrides() {
            return Err(HttpError::Forbidden(
                "ad-hoc transformation parameters are forbidden by origin policy (presets_only)"
                    .to_owned(),
            ));
        }
    }

    if let Some(name) = preset_name
        && !policy.is_preset_allowed(name)
    {
        return Err(HttpError::Forbidden(format!(
            "preset '{name}' is not permitted for this origin"
        )));
    }

    Ok(())
}

fn reject_backpressure(state: &AppState, reason: BackpressureReason) -> HttpError {
    state.metrics.record_backpressure_rejection(reason);
    HttpError::ServiceUnavailable {
        retry_after_secs: 2,
    }
}

async fn fetch_transform_and_cache(
    state: Arc<AppState>,
    origin_id: &'static str,
    origin_kind: OriginKind,
    source: String,
    spec: TransformSpec,
    key: CacheKey,
) -> Result<Arc<MissOutcome>, HttpError> {
    // Shed excess cache-miss load immediately before touching any origin so the
    // queue waiting for CPU transform slots stays strictly bounded.
    let pending_permit = Arc::clone(&state.pending_transform_permits)
        .try_acquire_owned()
        .map_err(|_| reject_backpressure(&state, BackpressureReason::PendingQueue))?;

    // Capture the source's invalidation epoch before fetching so a concurrent
    // `POST /cache/purge` or `POST /cache/cleanup` prevents stale writes to disk.
    let source_digest = source_digest_hex(origin_id, &source);
    let flight = Arc::new(state.invalidation.begin_flight(source_digest));

    // Reserve raw source memory incrementally *before* allocating buffers or
    // appending streamed chunks from any origin.
    let reserved_source =
        fetch_source_bytes(&state, origin_id, origin_kind, &source, &flight).await?;
    let original_size = reserved_source.bytes.len();

    // Bound concurrent CPU/memory-intensive image decoding and encoding, failing fast
    // with 503 Retry-After when the queue stays saturated past `transform_queue_timeout`.
    let transform_permit = tokio::time::timeout(
        state.transform_queue_timeout,
        Arc::clone(&state.transform_permits).acquire_owned(),
    )
    .await
    .map_err(|_| reject_backpressure(&state, BackpressureReason::TransformTimeout))?
    .map_err(|_| HttpError::Internal)?;
    let in_flight_transform = state.metrics.track_in_flight();

    let metrics = Arc::clone(&state.metrics);
    // Move all permits, byte reservations, and RAII gauges into the blocking task so
    // client cancellation cannot release concurrency or memory slots while CPU work runs.
    tokio::task::spawn_blocking(move || -> Result<Arc<MissOutcome>, HttpError> {
        let _pending_permit = pending_permit;
        let _transform_permit = transform_permit;
        let _in_flight_transform = in_flight_transform;

        let watermark = spec
            .watermark
            .as_ref()
            .and_then(|wm| state.watermarks.get(&wm.name));
        let transform_started = Instant::now();
        let output = engine::transform_with_watermark(
            &reserved_source.bytes,
            &spec,
            &state.engine_limits,
            watermark.as_deref(),
        )?;
        let transform_elapsed = transform_started.elapsed();

        // Release raw source bytes and their memory reservation as soon as transformation finishes.
        drop(reserved_source);

        // Store transformed bytes under a key matching the actual output format
        // (relevant if an animated source was passed through in its original format).
        let effective_key = if output.format == key.format() {
            key
        } else {
            let mut effective_spec = spec;
            effective_spec.format = output.format;
            CacheKey::from_parts(origin_id, &source, &effective_spec)
        };

        let etag = match state
            .invalidation
            .write_if_current(&flight, || state.cache.put(&effective_key, &output.bytes))
        {
            Some(Ok(stored)) => stored.etag,
            Some(Err(err)) => {
                metrics.record_cache_error(CacheTier::Variant, CacheOp::Write);
                tracing::warn!(
                    cache = "variant",
                    op = "write",
                    error = %err,
                    "variant cache write failed; serving uncached response"
                );
                compute_etag(&output.bytes)
            }
            None => {
                tracing::debug!(
                    origin = origin_id,
                    "skipping variant cache write for invalidated in-flight request"
                );
                compute_etag(&output.bytes)
            }
        };

        metrics.record_transform(origin_kind, output.format, transform_elapsed);

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

fn map_origin_fetch_error(state: &AppState, err: OriginError) -> HttpError {
    if matches!(err, OriginError::BudgetExhausted) {
        return reject_backpressure(state, BackpressureReason::ByteBudget);
    }
    HttpError::from(err)
}

/// Fetches raw source bytes while reserving memory against `source_byte_budget`
/// before allocation, coalescing concurrent remote fetches and consulting the
/// optional disk-backed `source_cache` for HTTP(S) and S3 origins.
async fn fetch_source_bytes(
    state: &Arc<AppState>,
    origin_id: &'static str,
    origin_kind: OriginKind,
    source: &str,
    flight: &Arc<FlightToken>,
) -> Result<Arc<ReservedSource>, HttpError> {
    if origin_kind == OriginKind::Local {
        let fetch_permit = acquire_fetch_permit(state).await?;
        let fetch_guard = state.metrics.track_in_flight_fetch();
        let local_origin = state.local_origin.clone().ok_or_else(|| {
            HttpError::Forbidden("local filesystem origin is disabled".to_owned())
        })?;
        let source_for_fs = source.to_owned();
        let mut reservation = state
            .source_byte_budget
            .begin(state.transform_queue_timeout);
        let (fetched, reservation) = tokio::task::spawn_blocking(move || {
            let _fetch_permit = fetch_permit;
            let _fetch_guard = fetch_guard;
            let fetched = local_origin.fetch_with_budget(&source_for_fs, &mut reservation)?;
            Ok::<_, OriginError>((fetched, reservation))
        })
        .await
        .map_err(|_| HttpError::Internal)?
        .map_err(|err| map_origin_fetch_error(state, err))?;

        return Ok(Arc::new(ReservedSource {
            bytes: Bytes::from(fetched.bytes),
            _reservation: reservation,
        }));
    }

    let digest = source_digest_hex(origin_id, source);

    if let Some(cached) = lookup_source_cache(state, &digest).await? {
        return Ok(cached);
    }

    let coalesced = state
        .source_coalescer
        .execute(digest.clone(), || {
            let state = Arc::clone(state);
            let source_owned = source.to_owned();
            let digest_for_put = digest.clone();
            let flight_for_put = Arc::clone(flight);
            async move {
                let fetch_permit = acquire_fetch_permit(&state).await?;
                let fetch_guard = state.metrics.track_in_flight_fetch();
                let mut reservation = state
                    .source_byte_budget
                    .begin(state.transform_queue_timeout);
                let fetched = match origin_kind {
                    OriginKind::Http => {
                        let http_origin = state.http_origin.as_ref().ok_or_else(|| {
                            HttpError::Forbidden("remote HTTP(S) origins are disabled".to_owned())
                        })?;
                        http_origin
                            .fetch_with_budget(&source_owned, &mut reservation)
                            .await
                            .map_err(|err| map_origin_fetch_error(&state, err))?
                    }
                    OriginKind::S3 => {
                        let s3_origin = state.s3_origin.as_ref().ok_or_else(|| {
                            HttpError::Forbidden("S3 origin is disabled".to_owned())
                        })?;
                        s3_origin
                            .fetch_with_budget(&source_owned, &mut reservation)
                            .await
                            .map_err(|err| map_origin_fetch_error(&state, err))?
                    }
                    OriginKind::Local => return Err(HttpError::Internal),
                };
                drop(fetch_guard);
                drop(fetch_permit);

                let bytes = Bytes::from(fetched.bytes);
                store_source_cache(&state, digest_for_put, bytes.clone(), flight_for_put).await;
                Ok(Arc::new(ReservedSource {
                    bytes,
                    _reservation: reservation,
                }))
            }
        })
        .await;

    coalesced.value
}

async fn acquire_fetch_permit(
    state: &AppState,
) -> Result<tokio::sync::OwnedSemaphorePermit, HttpError> {
    tokio::time::timeout(
        state.transform_queue_timeout,
        Arc::clone(&state.fetch_permits).acquire_owned(),
    )
    .await
    .map_err(|_| {
        state
            .metrics
            .record_backpressure_rejection(BackpressureReason::FetchTimeout);
        HttpError::ServiceUnavailable {
            retry_after_secs: 2,
        }
    })?
    .map_err(|_| HttpError::Internal)
}

async fn lookup_source_cache(
    state: &AppState,
    digest: &str,
) -> Result<Option<Arc<ReservedSource>>, HttpError> {
    let Some(source_cache) = state.source_cache.clone() else {
        return Ok(None);
    };
    let digest_for_get = digest.to_owned();
    let mut reservation = state
        .source_byte_budget
        .begin(state.transform_queue_timeout);
    match tokio::task::spawn_blocking(move || {
        let maybe_bytes = source_cache.get_raw_with_budget(&digest_for_get, |total_bytes| {
            reservation
                .reserve_total_blocking(total_bytes)
                .map_err(|_| crate::cache::CacheError::BudgetExhausted)
        })?;
        Ok::<_, crate::cache::CacheError>(maybe_bytes.map(|bytes| (bytes, reservation)))
    })
    .await
    {
        Ok(Ok(Some((cached, reservation)))) => Ok(Some(Arc::new(ReservedSource {
            bytes: Bytes::from(cached),
            _reservation: reservation,
        }))),
        Ok(Ok(None)) => Ok(None),
        Ok(Err(crate::cache::CacheError::BudgetExhausted)) => {
            Err(reject_backpressure(state, BackpressureReason::ByteBudget))
        }
        Ok(Err(err)) => {
            state
                .metrics
                .record_cache_error(CacheTier::Source, CacheOp::Read);
            tracing::warn!(
                cache = "source",
                op = "read",
                error = %err,
                "source cache read failed; falling back to origin"
            );
            Ok(None)
        }
        Err(err) => {
            state
                .metrics
                .record_cache_error(CacheTier::Source, CacheOp::Read);
            tracing::warn!(
                cache = "source",
                op = "read",
                error = %err,
                "source cache read task failed; falling back to origin"
            );
            Ok(None)
        }
    }
}

async fn store_source_cache(
    state: &AppState,
    digest: String,
    bytes: Bytes,
    flight: Arc<FlightToken>,
) {
    let Some(source_cache) = state.source_cache.clone() else {
        return;
    };
    let invalidation = state.invalidation.clone();
    match tokio::task::spawn_blocking(move || {
        invalidation.write_if_current(&flight, || source_cache.put_raw(&digest, &bytes))
    })
    .await
    {
        Ok(Some(Ok(()))) => {}
        Ok(None) => {
            tracing::debug!("skipping source cache write for invalidated in-flight request");
        }
        Ok(Some(Err(err))) => {
            state
                .metrics
                .record_cache_error(CacheTier::Source, CacheOp::Write);
            tracing::warn!(
                cache = "source",
                op = "write",
                error = %err,
                "source cache write failed; continuing without caching raw source"
            );
        }
        Err(err) => {
            state
                .metrics
                .record_cache_error(CacheTier::Source, CacheOp::Write);
            tracing::warn!(
                cache = "source",
                op = "write",
                error = %err,
                "source cache write task failed"
            );
        }
    }
}

fn is_http_source(source: &str) -> bool {
    source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || source
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn is_s3_source(source: &str) -> bool {
    source
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("s3://"))
}
