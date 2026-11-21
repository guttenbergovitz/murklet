# Changelog

All notable changes to Murklet are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

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
