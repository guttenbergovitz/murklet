//! Lock-free, low-cardinality operational metrics rendered in Prometheus text format.
//!
//! Every label dimension is backed by a fixed enum rather than unbounded strings so
//! untrusted URLs or query values can never cause label-cardinality explosion.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::domain::OutputFormat;

/// Standard Prometheus text exposition media type (`v0.0.4`).
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Upper bounds (in milliseconds) for `murklet_transform_duration_seconds` histogram buckets.
const DURATION_BUCKETS_MS: [u64; 10] = [5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000];
const DURATION_BUCKET_LABELS: [&str; 10] = [
    "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5",
];

/// Cache disposition label for `murklet_load_requests_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Served directly from the filesystem cache.
    Hit,
    /// Missed the cache and executed the origin + engine pipeline.
    Miss,
    /// Shared the in-flight leader's result via request coalescing.
    Coalesced,
    /// Request failed before reaching the cache (for example `400` or `401`).
    None,
}

impl CacheOutcome {
    const ALL: [Self; 4] = [Self::Hit, Self::Miss, Self::Coalesced, Self::None];

    const fn index(self) -> usize {
        match self {
            Self::Hit => 0,
            Self::Miss => 1,
            Self::Coalesced => 2,
            Self::None => 3,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Coalesced => "coalesced",
            Self::None => "none",
        }
    }
}

/// Low-cardinality HTTP status label for `murklet_load_requests_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    /// `200 OK`.
    Ok200,
    /// `304 Not Modified`.
    NotModified304,
    /// Any `4xx` client error.
    ClientError4xx,
    /// Any `5xx` server/upstream error.
    ServerError5xx,
}

impl StatusClass {
    const ALL: [Self; 4] = [
        Self::Ok200,
        Self::NotModified304,
        Self::ClientError4xx,
        Self::ServerError5xx,
    ];

    /// Maps an HTTP status code onto its low-cardinality metric class.
    #[must_use]
    pub const fn from_status_code(code: u16) -> Self {
        match code {
            200 => Self::Ok200,
            304 => Self::NotModified304,
            400..=499 => Self::ClientError4xx,
            _ => Self::ServerError5xx,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Ok200 => 0,
            Self::NotModified304 => 1,
            Self::ClientError4xx => 2,
            Self::ServerError5xx => 3,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok200 => "200",
            Self::NotModified304 => "304",
            Self::ClientError4xx => "4xx",
            Self::ServerError5xx => "5xx",
        }
    }
}

/// Origin adapter kind label for `murklet_transforms_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginKind {
    /// Local filesystem origin.
    Local,
    /// Remote HTTP(S) origin.
    Http,
    /// S3-compatible object storage origin.
    S3,
}

impl OriginKind {
    const ALL: [Self; 3] = [Self::Local, Self::Http, Self::S3];

    const fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Http => 1,
            Self::S3 => 2,
        }
    }

    /// Returns the lowercase metric/log label for this origin kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Http => "http",
            Self::S3 => "s3",
        }
    }
}

/// Cache storage tier label for `murklet_cache_errors_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTier {
    /// Encoded variant cache (`[cache]`).
    Variant,
    /// Raw upstream source cache (`[cache.source]`).
    Source,
}

impl CacheTier {
    const ALL: [Self; 2] = [Self::Variant, Self::Source];

    const fn index(self) -> usize {
        match self {
            Self::Variant => 0,
            Self::Source => 1,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Variant => "variant",
            Self::Source => "source",
        }
    }
}

/// Cache operation label for `murklet_cache_errors_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOp {
    /// Cache read (`get` / `get_raw`).
    Read,
    /// Cache write (`put` / `put_raw`).
    Write,
}

impl CacheOp {
    const ALL: [Self; 2] = [Self::Read, Self::Write];

    const fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Write => 1,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Low-cardinality backpressure rejection reason label for `murklet_backpressure_rejections_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureReason {
    /// Admission rejected immediately because `limits.max_pending_transforms` was full.
    PendingQueue,
    /// Timed out waiting for an origin fetch slot (`limits.max_concurrent_fetches`).
    FetchTimeout,
    /// Rejected or timed out waiting for raw source byte budget (`limits.max_in_flight_source_bytes`).
    ByteBudget,
    /// Timed out waiting for a CPU transform slot (`limits.max_concurrent_transforms`).
    TransformTimeout,
}

impl BackpressureReason {
    const ALL: [Self; 4] = [
        Self::PendingQueue,
        Self::FetchTimeout,
        Self::ByteBudget,
        Self::TransformTimeout,
    ];

    const fn index(self) -> usize {
        match self {
            Self::PendingQueue => 0,
            Self::FetchTimeout => 1,
            Self::ByteBudget => 2,
            Self::TransformTimeout => 3,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::PendingQueue => "pending_queue",
            Self::FetchTimeout => "fetch_timeout",
            Self::ByteBudget => "byte_budget",
            Self::TransformTimeout => "transform_timeout",
        }
    }
}

const ALL_FORMATS: [OutputFormat; 5] = [
    OutputFormat::Jpeg,
    OutputFormat::Png,
    OutputFormat::Gif,
    OutputFormat::Webp,
    OutputFormat::Avif,
];

const fn format_index(format: OutputFormat) -> usize {
    match format {
        OutputFormat::Jpeg => 0,
        OutputFormat::Png => 1,
        OutputFormat::Gif => 2,
        OutputFormat::Webp => 3,
        OutputFormat::Avif => 4,
    }
}

/// Lock-free operational metrics registry shared across HTTP handlers.
#[derive(Debug, Default)]
pub struct Metrics {
    load_requests: [[AtomicU64; 4]; 4],
    transforms: [[AtomicU64; 5]; 3],
    in_flight_transforms: AtomicUsize,
    in_flight_fetches: AtomicUsize,
    in_flight_source_bytes: AtomicUsize,
    transform_buckets: [AtomicU64; 10],
    transform_count: AtomicU64,
    transform_sum_micros: AtomicU64,
    cache_cleanups: AtomicU64,
    cache_errors: [[AtomicU64; 2]; 2],
    backpressure_rejections: [AtomicU64; 4],
}

/// RAII guard tracking an active image transformation in [`Metrics::in_flight_transforms`].
pub struct InFlightGuard {
    metrics: Arc<Metrics>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics
            .in_flight_transforms
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// RAII guard tracking an active origin fetch in [`Metrics::in_flight_fetches`].
pub struct InFlightFetchGuard {
    metrics: Arc<Metrics>,
}

impl Drop for InFlightFetchGuard {
    fn drop(&mut self) {
        self.metrics
            .in_flight_fetches
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// RAII guard tracking raw source bytes held in memory in [`Metrics::in_flight_source_bytes`].
pub struct InFlightBytesGuard {
    metrics: Arc<Metrics>,
    bytes: usize,
}

impl Drop for InFlightBytesGuard {
    fn drop(&mut self) {
        self.metrics
            .in_flight_source_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

impl Metrics {
    /// Creates a zero-initialised metrics registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments `murklet_load_requests_total` for the given cache disposition and status class.
    pub fn record_load_request(&self, cache: CacheOutcome, status: StatusClass) {
        self.load_requests[cache.index()][status.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Records a completed image transformation and its processing duration.
    pub fn record_transform(&self, origin: OriginKind, format: OutputFormat, duration: Duration) {
        self.transforms[origin.index()][format_index(format)].fetch_add(1, Ordering::Relaxed);

        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.transform_count.fetch_add(1, Ordering::Relaxed);
        self.transform_sum_micros
            .fetch_add(micros, Ordering::Relaxed);

        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        for (idx, bound_ms) in DURATION_BUCKETS_MS.iter().enumerate() {
            if millis <= *bound_ms {
                self.transform_buckets[idx].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Increments `murklet_in_flight_transforms` and returns an RAII guard that
    /// decrements the gauge when dropped.
    #[must_use]
    pub fn track_in_flight(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight_transforms.fetch_add(1, Ordering::Relaxed);
        InFlightGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Increments `murklet_in_flight_fetches` and returns an RAII guard that
    /// decrements the gauge when dropped.
    #[must_use]
    pub fn track_in_flight_fetch(self: &Arc<Self>) -> InFlightFetchGuard {
        self.in_flight_fetches.fetch_add(1, Ordering::Relaxed);
        InFlightFetchGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Adds `bytes` to `murklet_in_flight_source_bytes` and returns an RAII guard
    /// that subtracts `bytes` when dropped.
    #[must_use]
    pub fn track_in_flight_source_bytes(self: &Arc<Self>, bytes: usize) -> InFlightBytesGuard {
        self.in_flight_source_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        InFlightBytesGuard {
            metrics: Arc::clone(self),
            bytes,
        }
    }

    /// Increments `murklet_cache_cleanups_total`.
    pub fn record_cache_cleanup(&self) {
        self.cache_cleanups.fetch_add(1, Ordering::Relaxed);
    }

    /// Increments `murklet_cache_errors_total` for the given cache tier and operation.
    pub fn record_cache_error(&self, tier: CacheTier, op: CacheOp) {
        self.cache_errors[tier.index()][op.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Increments `murklet_backpressure_rejections_total` for the given rejection reason.
    pub fn record_backpressure_rejection(&self, reason: BackpressureReason) {
        self.backpressure_rejections[reason.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Renders all metrics in Prometheus text exposition format (`v0.0.4`).
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let mut out = String::with_capacity(2048);

        out.push_str("# HELP murklet_load_requests_total Total GET /load requests by cache outcome and status class.\n");
        out.push_str("# TYPE murklet_load_requests_total counter\n");
        for cache in CacheOutcome::ALL {
            for status in StatusClass::ALL {
                let count =
                    self.load_requests[cache.index()][status.index()].load(Ordering::Relaxed);
                if count > 0 {
                    let _ = writeln!(
                        out,
                        "murklet_load_requests_total{{cache=\"{}\",status=\"{}\"}} {count}",
                        cache.as_str(),
                        status.as_str()
                    );
                }
            }
        }

        out.push_str("# HELP murklet_transforms_total Completed image transformations by origin and output format.\n");
        out.push_str("# TYPE murklet_transforms_total counter\n");
        for origin in OriginKind::ALL {
            for format in ALL_FORMATS {
                let count =
                    self.transforms[origin.index()][format_index(format)].load(Ordering::Relaxed);
                if count > 0 {
                    let _ = writeln!(
                        out,
                        "murklet_transforms_total{{origin=\"{}\",format=\"{}\"}} {count}",
                        origin.as_str(),
                        format.as_str()
                    );
                }
            }
        }

        let in_flight = self.in_flight_transforms.load(Ordering::Relaxed);
        out.push_str("# HELP murklet_in_flight_transforms Currently executing CPU-bound image transformations.\n");
        out.push_str("# TYPE murklet_in_flight_transforms gauge\n");
        let _ = writeln!(out, "murklet_in_flight_transforms {in_flight}");

        let in_flight_fetches = self.in_flight_fetches.load(Ordering::Relaxed);
        out.push_str("# HELP murklet_in_flight_fetches Currently executing origin fetches.\n");
        out.push_str("# TYPE murklet_in_flight_fetches gauge\n");
        let _ = writeln!(out, "murklet_in_flight_fetches {in_flight_fetches}");

        let in_flight_bytes = self.in_flight_source_bytes.load(Ordering::Relaxed);
        out.push_str("# HELP murklet_in_flight_source_bytes Raw source payload bytes currently held in memory awaiting or undergoing transformation.\n");
        out.push_str("# TYPE murklet_in_flight_source_bytes gauge\n");
        let _ = writeln!(out, "murklet_in_flight_source_bytes {in_flight_bytes}");

        let total_count = self.transform_count.load(Ordering::Relaxed);
        let sum_micros = self.transform_sum_micros.load(Ordering::Relaxed);
        let sum_secs_whole = sum_micros / 1_000_000;
        let sum_secs_frac = sum_micros % 1_000_000;

        out.push_str("# HELP murklet_transform_duration_seconds Duration of CPU-bound image decoding, transformation, and encoding in seconds.\n");
        out.push_str("# TYPE murklet_transform_duration_seconds histogram\n");
        for (idx, label) in DURATION_BUCKET_LABELS.iter().enumerate() {
            let bucket_count = self.transform_buckets[idx].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "murklet_transform_duration_seconds_bucket{{le=\"{label}\"}} {bucket_count}"
            );
        }
        let _ = writeln!(
            out,
            "murklet_transform_duration_seconds_bucket{{le=\"+Inf\"}} {total_count}"
        );
        let _ = writeln!(
            out,
            "murklet_transform_duration_seconds_sum {sum_secs_whole}.{sum_secs_frac:06}"
        );
        let _ = writeln!(
            out,
            "murklet_transform_duration_seconds_count {total_count}"
        );

        self.render_cache_and_backpressure_metrics(&mut out);

        out
    }

    fn render_cache_and_backpressure_metrics(&self, out: &mut String) {
        let cleanups = self.cache_cleanups.load(Ordering::Relaxed);
        out.push_str("# HELP murklet_cache_cleanups_total Total successful POST /cache/cleanup executions.\n");
        out.push_str("# TYPE murklet_cache_cleanups_total counter\n");
        let _ = writeln!(out, "murklet_cache_cleanups_total {cleanups}");

        out.push_str("# HELP murklet_cache_errors_total Total filesystem cache read and write errors by tier and operation.\n");
        out.push_str("# TYPE murklet_cache_errors_total counter\n");
        for tier in CacheTier::ALL {
            for op in CacheOp::ALL {
                let count = self.cache_errors[tier.index()][op.index()].load(Ordering::Relaxed);
                if count > 0 {
                    let _ = writeln!(
                        out,
                        "murklet_cache_errors_total{{cache=\"{}\",op=\"{}\"}} {count}",
                        tier.as_str(),
                        op.as_str()
                    );
                }
            }
        }

        out.push_str("# HELP murklet_backpressure_rejections_total Total 503 backpressure rejections by resource-limit reason.\n");
        out.push_str("# TYPE murklet_backpressure_rejections_total counter\n");
        for reason in BackpressureReason::ALL {
            let count = self.backpressure_rejections[reason.index()].load(Ordering::Relaxed);
            if count > 0 {
                let _ = writeln!(
                    out,
                    "murklet_backpressure_rejections_total{{reason=\"{}\"}} {count}",
                    reason.as_str()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_prometheus_metrics_and_tracks_in_flight_guard() {
        let metrics = Arc::new(Metrics::new());

        metrics.record_load_request(CacheOutcome::Miss, StatusClass::Ok200);
        metrics.record_load_request(CacheOutcome::Hit, StatusClass::NotModified304);
        metrics.record_transform(
            OriginKind::Local,
            OutputFormat::Webp,
            Duration::from_millis(42),
        );
        metrics.record_cache_cleanup();
        metrics.record_cache_error(CacheTier::Variant, CacheOp::Read);
        metrics.record_cache_error(CacheTier::Source, CacheOp::Write);
        metrics.record_backpressure_rejection(BackpressureReason::PendingQueue);
        metrics.record_backpressure_rejection(BackpressureReason::ByteBudget);

        let guard = metrics.track_in_flight();
        let fetch_guard = metrics.track_in_flight_fetch();
        let bytes_guard = metrics.track_in_flight_source_bytes(4096);
        let during = metrics.render_prometheus();
        assert!(during.contains("murklet_in_flight_transforms 1\n"));
        assert!(during.contains("murklet_in_flight_fetches 1\n"));
        assert!(during.contains("murklet_in_flight_source_bytes 4096\n"));
        drop(guard);
        drop(fetch_guard);
        drop(bytes_guard);

        let after = metrics.render_prometheus();
        assert!(after.contains("murklet_in_flight_transforms 0\n"));
        assert!(after.contains("murklet_in_flight_fetches 0\n"));
        assert!(after.contains("murklet_in_flight_source_bytes 0\n"));
        assert!(after.contains("murklet_load_requests_total{cache=\"miss\",status=\"200\"} 1\n"));
        assert!(after.contains("murklet_load_requests_total{cache=\"hit\",status=\"304\"} 1\n"));
        assert!(after.contains("murklet_transforms_total{origin=\"local\",format=\"webp\"} 1\n"));
        assert!(after.contains("murklet_transform_duration_seconds_bucket{le=\"0.025\"} 0\n"));
        assert!(after.contains("murklet_transform_duration_seconds_bucket{le=\"0.05\"} 1\n"));
        assert!(after.contains("murklet_transform_duration_seconds_sum 0.042000\n"));
        assert!(after.contains("murklet_transform_duration_seconds_count 1\n"));
        assert!(after.contains("murklet_cache_cleanups_total 1\n"));
        assert!(after.contains("murklet_cache_errors_total{cache=\"variant\",op=\"read\"} 1\n"));
        assert!(after.contains("murklet_cache_errors_total{cache=\"source\",op=\"write\"} 1\n"));
        assert!(
            after.contains("murklet_backpressure_rejections_total{reason=\"pending_queue\"} 1\n")
        );
        assert!(
            after.contains("murklet_backpressure_rejections_total{reason=\"byte_budget\"} 1\n")
        );
    }
}
