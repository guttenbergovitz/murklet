# Changelog

All notable changes to Murklet are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.7.2] - 2026-11-22

### Changed

- **Application orchestration extracted from HTTP router (`src/application/load.rs`, `src/http/router.rs`):** Moved policy resolution, cache lookup, singleflight coalescing, resource-isolation guards, origin fetching, image transformation, and cache invalidation orchestration out of `src/http/router.rs` into `src/application/load.rs`. `src/http/router.rs` now handles HTTP transport concerns only (routing, authentication checks, `ETag`/`If-None-Match`, diagnostic headers, and response formatting) while re-exporting `AppState` for backwards compatibility.

## [0.7.1] - 2026-11-22

### Fixed

- **Linearisable cache invalidation against in-flight writes (`src/cache/invalidation.rs`, `src/http/router.rs`):** Added an RAII `InvalidationTracker` (`FlightToken` + shared/exclusive I/O gate) so `POST /cache/purge` and `POST /cache/cleanup` advance per-source or global generation epochs and prevent concurrent in-flight fetch/transform requests from repopulating stale variant or raw-source entries on disk after invalidation.

## [0.7.0] - 2026-11-22

### Added

- **Bounded fetch concurrency, pending queue, and in-flight source memory budget (`src/policy/config.rs`, `src/http/router.rs`, `src/main.rs`):** Added `limits.max_concurrent_fetches` (default `32`), `limits.max_pending_transforms` (default `64`, shedding excess cache-miss load with `503 Retry-After` before touching any origin), and `limits.max_in_flight_source_bytes` (default `256 MiB`, bounding aggregate raw source bytes held in memory while waiting for or executing image transformations).

## [0.6.1] - 2026-11-22

### Fixed

- **Cache read and write error observability (`src/http/metrics.rs`, `src/http/router.rs`):** Stopped silently treating filesystem cache read/write errors (`FsCache::get`, `FsCache::put`, `FsCache::get_raw`, `FsCache::put_raw`) as ordinary cache misses. Requests still degrade gracefully to origin, while emitting structured `tracing::warn!` logs and incrementing the low-cardinality Prometheus counter `murklet_cache_errors_total{cache="variant|source",op="read|write"}`.

## [0.6.0] - 2026-11-21

### Added

- **Per-origin policies (`src/policy/origin_policy.rs`, `src/http/router.rs`, `src/http/explain.rs`):** Added declarative `[origins.<kind>.policy]` tables supporting `presets_only`, `allowed_presets`, `allowed_formats` (intersected with `Accept` negotiation), `max_target_dimension`, `default_watermark`, and per-origin `cache_control` overrides per ADR 0003.
- **Selective per-source cache purge (`src/cache/fs.rs`, `src/http/router.rs`):** Updated `FsCache` to a source-bucketed layout (`<shard>/<source_digest>/<variant_key>.<ext>`) and added `POST /cache/purge?url=<source>` (plus `?url=<source>` on `POST /cache/cleanup`) for $O(1)$ per-source variant and raw-source cache invalidation.
- **Offline policy test runner (`src/http/policy_test.rs`, `src/main.rs`):** Added `murklet test-policy <suite.toml> [config.toml]` to evaluate declarative TOML `[[case]]` assertions (`expect = "allow" | "deny"`, origin, preset, format, dimensions, watermark, `vary_accept`, and `reason_contains`) in CI without starting the server.

## [0.5.0] - 2026-11-21

### Added

- **Creative post-resize filters (`src/domain/filters.rs`, `src/engine/pipeline.rs`):** Added `duotone` (`<dark_hex>:<light_hex>`), `posterize` (`2..=16`), `threshold` (`0..=255`), `pixelate` (`2..=64`), and `vignette` (`0..=100`) in Murklet's fixed canonical pipeline order, wired into `[presets.*]` and `murklet explain`.
- **Policy-controlled watermarks (`src/domain/watermark.rs`, `src/engine/watermark.rs`, `src/policy/config.rs`):** Added declarative `[watermarks]` mapping validated logical names to local image files loaded and decoded once at startup under `EngineLimits` (zero runtime I/O, zero SSRF or path traversal), with `wm`/`watermark`, `wm_pos` (with 2% edge margin for non-centred anchors), `wm_opacity` (`1..=100`), and `wm_scale` (`5..=100`) query and preset parameters and SIMD `fast_image_resize` + Porter-Duff `source-over` compositing.
- **Production hardening (`src/cache/fs.rs`, `src/http/router.rs`, `src/main.rs`):** Added optional raw-source cache (`[cache.source]`) for remote HTTP(S) and S3 origins, transform queue backpressure (`limits.transform_queue_timeout_secs` returning `503 Service Unavailable` with `Retry-After`), and the `murklet sign <url> [config]` CLI command.

## [0.4.0] - 2026-11-21

### Added

- **Pure-Rust S3-compatible origin (`src/origin/s3.rs`):** Added `S3Origin` with AWS Signature Version 4 (`SigV4`) signing, custom endpoint and path-style support (AWS S3, Cloudflare R2, MinIO, RustFS, Hetzner, etc.), optional object key prefix, environment-variable credential fallback (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), and `s3://` routing across `/load`, `/metrics`, and `murklet explain`.
- **Fit, crop, geometry, DPR, and alpha flattening (`src/domain/geometry.rs`, `src/engine/pipeline.rs`):** Added `fit` (`fill`, `contain`, `cover`, `scale-down`), `gravity` (9 anchor points plus focal point `fp:x:y`), clockwise `rotate` (`0`, `90`, `180`, `270`), `dpr` (`1.0..=3.0`), and `bg` RGB hex background flattening for alpha-bearing images encoded to JPEG.
- **Post-resize image filters & brand monochrome tint (`src/domain/filters.rs`, `src/engine/pipeline.rs`):** Added a fixed canonical post-resize filter pipeline supporting `flip` (`h`/`v`/`hv`), `brightness` (`-100..=100`), `contrast` (`-100..=100`), `saturation` (`-100..=100`), `hue` (`-360..=360`), `gamma` (`0.2..=5.0` via 256-byte LUT), `grayscale`/`greyscale`, `sepia` (`0..=100` or `true`/`false`), brand monochrome duotone `tint`/`mono`/`monochrome` (RGB hex `black -> base colour -> white` ramp preserving alpha), `invert`/`negative`, `blur` (`0.5..=20.0`), and `sharpen` (`0.5..=10.0`), all wired into `[presets.*]` and `murklet explain`.
- **Cache TTL & disk-budget eviction (`src/cache/fs.rs`, `src/policy/config.rs`, `src/main.rs`):** Added optional `cache.max_total_bytes`, `cache.ttl_secs`, and `cache.sweep_interval_secs` with lazy TTL expiry on reads, oldest-`mtime` eviction sweeps ignoring in-flight temp files, empty shard pruning, and a graceful background sweeper task.

## [0.3.0] - 2026-11-21

### Added

- **Structured logging & Prometheus metrics (`src/http/metrics.rs`):** Initialised `tracing-subscriber` with configurable `server.log_format` (`pretty` / `json`) and `RUST_LOG` filtering, and exposed lock-free, low-cardinality Prometheus metrics (`murklet_load_requests_total`, `murklet_transforms_total`, `murklet_in_flight_transforms`, `murklet_transform_duration_seconds`, `murklet_cache_cleanups_total`) at `GET /metrics` (controlled by `server.metrics_enabled`).
- **Declarative named presets (`src/policy/presets.rs`):** Added validated `[presets.<name>]` sections in `murklet.toml`, `?preset=<name>` support on `GET /load` with explicit query overrides, and shared canonical `CacheKey` identities between preset and equivalent explicit queries.
- **Policy inspection CLI (`src/http/explain.rs`):** Added `murklet explain [--accept <header>] <load-url-or-query> [config]` to evaluate signatures, origins, presets, limits, and canonical cache keys offline without starting the server.
- **SIMD resampling & benchmarks (`src/engine/pipeline.rs`, `benches/engine_resize.rs`):** Replaced `image`'s scalar resampler with SIMD-accelerated `fast_image_resize` (3-channel `Rgb8` for opaque images, 4-channel `Rgba8` for alpha images, exact kernel mappings) and added reproducible Criterion benchmarks via `task bench`.

## [0.2.0] - 2026-11-21

### Added

- **Pure-Rust AVIF output encoding (`src/engine`):** Enabled the pure-Rust `avif` feature (`ravif`/`rav1e`) in the `image` crate with speed preset `8` for on-demand HTTP delivery and quality (`1..=100`) support.
- **AVIF `Accept` negotiation (`src/http`):** Updated `negotiate_format` to prefer `AVIF > WebP > JPEG > PNG` when `server.accept_negotiation = true` and `f` is omitted.
- **Developer workflow (`Taskfile.yml`):** Added `Taskfile.yml` with `check`, `fmt`, `lint`, `test`, `config:check`, `dev`, `dev:release` (recommended for AVIF), `smoke` (healthz, MISS, HIT, `304`), and `docker:build`, and ignored local `murklet.toml` and `/assets/` in `.gitignore`.

## [0.1.0] - 2026-11-21

### Added

- **Domain layer (`src/domain`):** Validated transformation value types (`Dimension`, `ImageDimensions`, `TargetDimensions`, `Quality`, `OutputFormat`, `ResizeFilter`, `TransformSpec`) with overflow-safe proportional scaling and versioned canonical representations.
- **Pure-Rust image engine (`src/engine`):** Header-level dimension and decoded-pixel guards (decompression bomb defence), EXIF orientation handling, resampling, JPEG/PNG/WebP/GIF encoding, and configurable animated input fallback (`first_frame` / `passthrough`) per ADR 0001.
- **Origins (`src/origin`):**
  - Hardened local filesystem origin (`LocalFsOrigin`) with lexical traversal rejection, canonicalised symlink containment, open-handle regular-file checks, and bounded streaming reads.
  - Hardened HTTP(S) origin (`HttpOrigin`) with deny-by-default `HostAllowlist`, IPv4/IPv6/IPv4-mapped/NAT64 SSRF blocking, DNS-pinning `SafeDnsResolver`, manual per-hop redirect validation, timeouts, and bounded streaming reads.
- **Caching (`src/cache`):** Sharded filesystem variant cache (`FsCache`) with length-prefixed BLAKE3 canonical keys, crash-safe atomic writes, strong content-based ETags, full cleanup, and cancellation-safe async request coalescing (`Coalescer`).
- **Declarative policy & signing (`src/policy`):** Strictly validated TOML configuration (`murklet.example.toml`, `deny_unknown_fields`, `--check` CLI validation mode) and optional constant-time HMAC-SHA256 request signing on `GET /load`.
- **HTTP server (`src/http`):** Axum transport exposing `GET /healthz`, `GET /load` (with `ETag`, `If-None-Match` `304`, `Cache-Control`, `X-Cache`, optional `Accept`/`Vary` format negotiation, optional diagnostic headers, zero-copy `Bytes` sharing, and semaphore-bounded transform concurrency), and `POST /cache/cleanup`.
- **Tooling & CI:** GitHub Actions workflow (`cargo fmt`, strict `clippy`, `cargo test`), multi-stage `Dockerfile`, ADRs 0001–0002, engineering conventions, and canonical project skills in `.agents/skills/`.
