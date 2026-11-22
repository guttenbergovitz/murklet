# Murklet

Images under control.

Murklet is an open-source image proxy and on-demand image transformation service, written in Rust.

It fetches images from trusted sources, transforms and optimises them, caches the results, and serves them efficiently over HTTP. Its emphasis is straightforward: dependable image delivery first, backed by explicit policies, predictable caching, and practical observability.

> **Project status:** `v0.7.0` (early release). Implemented and tested: local filesystem, SSRF-hardened HTTP(S), and pure-Rust S3-compatible (`SigV4`) origins; per-origin transformation and delivery policies (`[origins.<kind>.policy]` with `presets_only`, preset/format allowlists, dimension caps, default watermarks, and `Cache-Control` overrides); pure-Rust static image engine with SIMD `fast_image_resize` (JPEG, PNG, WebP, GIF, and AVIF), fit/cover/contain/scale-down cropping with gravity & focal points, rotation, DPR, alpha flattening, post-resize colour/tonal/monochrome & creative filters (`duotone`, `posterize`, `threshold`, `pixelate`, `vignette`), and policy-controlled watermarks (`[watermarks]`); source-bucketed filesystem variant & optional raw-source cache with request coalescing, TTL expiry, disk-budget eviction, and $O(1)$ selective per-source invalidation (`POST /cache/purge`); transform queue backpressure (`503 Retry-After`); declarative TOML configuration with named presets, `murklet explain`, `murklet test-policy`, and `murklet sign`; optional HMAC-SHA256 request signing; `Accept` format negotiation; structured `tracing` logs; Prometheus `/metrics`; and Criterion benchmarks. Path-based URL routes and OpenTelemetry exporters remain planned.

## Motivation

Murklet exists for two equally candid reasons:

1. **Hands-on engineering in Rust:** It is a deliberate vehicle for working directly with lower-level Rust concepts — ownership and zero-copy boundaries, bounded concurrency, async I/O, FFI trade-offs, and resource isolation — on a real networked service rather than toy exercises.
2. **Real operational friction in existing proxies:** Mature image proxies already exist, yet operating them in practice still exposes recurring pain points: sprawling environment-variable configuration that is hard to audit or test, weak separation of policies per origin, and the need to bolt on an external caching tier even for modest deployments.

Murklet is not trying to differentiate itself merely by being written in Rust, supporting AVIF, or publishing synthetic benchmarks. The aim is a small, dependable image proxy whose behaviour is easy to control, explain, test, and operate:

- **Image proxy first:** Fetching, resizing, cropping, encoding, caching, and HTTP delivery are the core product.
- **Multiple origins:** Image sources are adapters behind a narrow contract, not assumptions baked into the HTTP or processing layers.
- **Secure by default:** Trusted origins, per-origin transformation boundaries, signed requests where appropriate, and bounded use of CPU, memory, network, and storage.
- **Predictable caching:** Canonical transformation specifications, versioned cache identities, and self-contained selective invalidation.
- **Observable decisions:** Offline policy inspection and CI verification (`explain`, `test-policy`), structured logs, and Prometheus metrics without mandating a heavy observability stack.
- **Simple deployment:** A single service that is useful without a database, external caching tier, Kubernetes, or a control plane.

## Capabilities

### Image sources

Murklet is not tied to S3. The origin interface supports different source types without altering the image-processing pipeline, and each origin can enforce its own `[origins.<kind>.policy]` rules.

| Source | Status | Role |
| --- | --- | --- |
| Local filesystem (`[origins.local]`) | Implemented | Serve assets from configured directories with lexical traversal rejection, canonicalised symlink containment, and open-handle file verification. |
| HTTP(S) (`[origins.http]`) | Implemented | Fetch images from explicitly allowlisted remote hosts (`http://`, `https://`) with DNS-pinning SSRF defence and per-hop redirect validation. Disabled by default. |
| S3-compatible storage (`[origins.s3]`) | Implemented | Fetch objects via `s3://` using pure-Rust AWS Signature Version 4 (`SigV4`), supporting AWS S3, Cloudflare R2, MinIO, RustFS, and custom endpoints/prefixes. |
| Additional cloud / custom origins | Planned | Dedicated GCS or Azure Blob adapters behind the origin contract if native provider behaviour warrants them beyond S3 compatibility. |

### Transformations

- **Geometry and layout:** On-demand resize (`w`, `h`), fit modes (`fill`, `contain`, `cover`, `scale-down`), 9-point anchor gravity and normalised focal-point cropping (`fp:x:y`), clockwise rotation (`0`, `90`, `180`, `270`), device pixel ratio (`dpr` `1.0..=3.0`), and resampling kernel selection (`nearest`, `box`, `linear`, `hermite`, `catmullrom`, `bspline`, `gaussian`, `lanczos`).
- **Formats and encoding:** Pure-Rust JPEG, PNG, WebP, GIF, and AVIF output with quality (`q` `1..=100`), alpha background flattening (`bg`), EXIF orientation handling, and configurable animated input handling (`first_frame` or `passthrough`).
- **Format negotiation:** Optional HTTP `Accept` header negotiation (`AVIF > WebP > JPEG > PNG`, intersected with per-origin `allowed_formats`) with automatic `Vary: Accept` and distinct canonical cache keys.
- **Post-resize filter pipeline:** Fixed canonical pipeline supporting `flip` (`h`/`v`/`hv`), `brightness`, `contrast`, `saturation`, `hue`, `gamma`, `grayscale`/`greyscale`, `sepia`, brand monochrome duotone (`tint`/`mono`), `duotone` (`<dark_hex>:<light_hex>`), `invert`/`negative`, `posterize`, `threshold`, `pixelate`, `blur`, `sharpen`, and `vignette`.
- **Policy-controlled watermarks:** Declarative `[watermarks]` mapping logical names to local assets decoded once at startup (zero runtime file I/O or SSRF risk), applied via `wm`, `wm_pos`, `wm_opacity`, and `wm_scale` or per-origin `default_watermark`.

### Cache and delivery

- **Two-tier filesystem cache:** Source-bucketed variant cache (`<shard>/<source_digest>/<variant_key>.<ext>`) plus an optional raw-source cache (`[cache.source]`) so multiple variants of the same remote HTTP(S) or S3 asset share a single upstream fetch.
- **Canonical cache identities:** Versioned, length-prefixed BLAKE3 cache keys where named presets (`?preset=thumb`) and equivalent explicit queries share the same cache entry.
- **Stampede protection:** Cancellation-safe async request coalescing for both variant transformations and remote source fetches.
- **Lifecycle and eviction:** Configurable TTL expiry (`ttl_secs`), disk-budget eviction (`max_total_bytes`) via oldest-`mtime` background sweeps, and crash-safe atomic writes.
- **Selective and full invalidation:** Token-authenticated `POST /cache/purge?url=<source>` (and `POST /cache/cleanup?url=<source>`) for $O(1)$ per-source variant and raw-source eviction, alongside full cache cleanup (`POST /cache/cleanup`).
- **HTTP semantics:** Strong content-based `ETag`, `If-None-Match` (`304 Not Modified`), global and per-origin `Cache-Control`, `X-Cache` (`HIT`/`MISS`), and optional `X-Original-*` / `X-Processed-*` diagnostic headers.

### Security and reliability

- **Origin isolation:** Deny-by-default remote origins, strict host allowlists (`exact` and `*.domain`), and optional constant-time HMAC-SHA256 request signing (`server.signing_secret` + `murklet sign`).
- **SSRF and path-traversal defences:** IPv4, IPv6, IPv4-mapped, and NAT64 private/loopback/link-local/metadata range blocking, DNS IP pinning across redirects, and canonicalised filesystem containment.
- **Decompression-bomb and resource guards:** Header-level source dimension and decoded-pixel checks before allocation, bounded streaming reads (`max_input_bytes`), and separate backpressure limits for pending cache-miss admission (`max_pending_transforms`), origin fetch concurrency (`max_concurrent_fetches`), aggregate in-flight source memory (`max_in_flight_source_bytes`), and CPU-bound transform concurrency (`max_concurrent_transforms`) returning `503 Service Unavailable` with `Retry-After`.
- **Memory safety:** `#![forbid(unsafe_code)]` across the crate and a pure-Rust default image pipeline (see [ADR 0001](docs/adr/0001-pure-rust-static-image-engine.md)).

### Policies, verification, and observability

- **Declarative TOML configuration:** Strictly validated at startup (`#[serde(deny_unknown_fields)]`) or offline via `murklet --check`.
- **Named presets and per-origin boundaries:** Reusable `[presets.<name>]` definitions and `[origins.<kind>.policy]` controls (`presets_only`, `allowed_presets`, `allowed_formats`, `max_target_dimension`, `default_watermark`, and `cache_control`) so public origins can be locked down without mandatory URL signing (see [ADR 0003](docs/adr/0003-policy-first-differentiation-and-per-origin-boundaries.md)).
- **Offline explainability and CI policy testing:** `murklet explain` inspects how any request resolves against signatures, origin rules, presets, format negotiation, and cache keys; `murklet test-policy` runs declarative TOML test suites (`[[case]]`) in CI to catch policy regressions before deployment.
- **Telemetry:** Structured `tracing` logs (`pretty` or `json` with `RUST_LOG` filtering) and lock-free, low-cardinality Prometheus metrics at `GET /metrics` (`murklet_load_requests_total`, `murklet_transforms_total`, `murklet_in_flight_transforms`, `murklet_transform_duration_seconds`, `murklet_cache_cleanups_total`, `murklet_cache_errors_total`).

## HTTP API & Transformation Reference

### Endpoints

| Method & Path | Purpose | Authentication / Notes |
| --- | --- | --- |
| `GET /load?<params>` | Fetch, transform, cache, and serve an image variant. | Optional `&sig=<hex>` when `server.signing_secret` is configured. Supports `Accept` negotiation and `If-None-Match`. |
| `GET /healthz` | Liveness / readiness check (`200 OK`, body `ok`). | Public. |
| `GET /metrics` | Prometheus text-format metrics (`version=0.0.4`). | Controlled by `server.metrics_enabled` (`404` when disabled). |
| `POST /cache/purge?url=<source>` | Evict all cached variants (and raw source entry) for a single `<source>` in $O(1)$. | Requires `X-Cache-Cleanup-Token: <token>` matching `server.cleanup_token` (fails closed with `401` if unset). |
| `POST /cache/cleanup` | Evict the entire variant and raw-source cache (or a single source when `?url=<source>` is passed). | Requires `X-Cache-Cleanup-Token: <token>` matching `server.cleanup_token`. |

### `GET /load` query parameters

All transformation parameters below (except `url`, `preset`, and `sig`) can also be declared in `[presets.<name>]` in `murklet.toml`. When a request passes `?preset=<name>`, explicit query parameters override the preset's defaults unless the origin enforces `presets_only = true`.

#### Source, preset & signature

| Parameter | Type / Values | Default | Description |
| --- | --- | --- | --- |
| `url` | `<relative-path>` \| `https://...` \| `s3://...` | *required* | Source asset: relative path (`[origins.local]`), remote URL (`[origins.http]`), or S3 object URI (`[origins.s3]`). |
| `preset` | `<name>` (`[a-zA-Z0-9_-]+`) | — | Apply a named preset declared in `[presets.<name>]`. |
| `sig` | 64-char hex HMAC-SHA256 | — | Request signature over the raw query string (required when `server.signing_secret` is set; generate via `murklet sign`). |

#### Dimensions, fit, geometry & encoding

| Parameter | Values / Range | Default | Description |
| --- | --- | --- | --- |
| `w` | `0..=max_target_dimension` (`0` = unset) | original | Target width in pixels (scaled proportionally when `h` is omitted). |
| `h` | `0..=max_target_dimension` (`0` = unset) | original | Target height in pixels (scaled proportionally when `w` is omitted). |
| `dpr` | `1.0..=3.0` | `1.0` | Device pixel ratio multiplier applied to `w` and `h` (still bounded by `max_target_dimension`). |
| `fit` | `fill`, `contain`, `cover`, `scale-down` | `fill` | `fill` stretches when both `w` & `h` are set; `contain` fits inside `w×h`; `cover` fills `w×h` and crops overflow using `gravity`; `scale-down` behaves like `contain` without upscaling. |
| `gravity` | `center` (`ce`), `north` (`no`/`top`), `south` (`so`/`bottom`), `east` (`ea`/`right`), `west` (`we`/`left`), `nw` (`north-west`), `ne` (`north-east`), `sw` (`south-west`), `se` (`south-east`), or `fp:<x>:<y>` (`0.0..=1.0`) | `center` | Crop anchor or normalised focal point used when `fit=cover` (for example `gravity=fp:0.35:0.2`). |
| `rotate` | `0`, `90`, `180`, `270` | `0` | Clockwise rotation in degrees applied after EXIF orientation normalisation. |
| `filter` | `nearest`, `box`, `linear`, `hermite`, `catmullrom`, `bspline`, `gaussian`, `lanczos` | auto (`lanczos` downscale, `catmullrom` upscale) | Resampling kernel used by SIMD `fast_image_resize`. |
| `f` | `jpg` / `jpeg`, `png`, `gif`, `webp`, `avif` | `jpeg` (or `Accept` negotiated) | Output encoding format. When omitted and `server.accept_negotiation = true`, negotiated from `Accept`. |
| `q` | `1..=100` | `75` | Lossy encoding quality for `jpeg`, `webp`, and `avif` (ignored in cache keys for lossless `png`/`gif`). |
| `bg` | 3- or 6-digit RGB hex (e.g. `fff`, `f5f5f5`, `#1a1a1a`) | — | Flattens alpha transparency onto a solid background colour before encoding. |

#### Post-resize colour, tonal & creative filters

Filters execute in a fixed canonical order after resizing so parameter ordering in the URL never fragments the cache:

| Parameter (Aliases) | Values / Range | Default | Description |
| --- | --- | --- | --- |
| `flip` | `h`, `v`, `hv` (`vh`, `both`) | none | Horizontal, vertical, or dual-axis mirror flip. |
| `brightness` | `-100..=100` | `0` | Additive luminance shift. |
| `contrast` | `-100..=100` | `0` | Midpoint contrast adjustment. |
| `saturation` | `-100..=100` | `0` | Colour saturation adjustment (`-100` removes chroma). |
| `hue` | `-360..=360` | `0` | Hue rotation in degrees. |
| `gamma` | `0.2..=5.0` | `1.0` | Tonal gamma correction via 256-entry LUT. |
| `grayscale` (`greyscale`) | `true`, `false`, `1`, `0` | `false` | Converts the image to greyscale luma. |
| `sepia` | `0..=100` (or `true`=`100`, `false`=`0`) | `0` | Sepia tone blend intensity. |
| `tint` (`mono`, `monochrome`) | 3- or 6-digit RGB hex (e.g. `1a73e8`) | — | Brand monochrome ramp mapping `black -> tint -> white` while preserving alpha. |
| `duotone` | `<dark_hex>:<light_hex>` (e.g. `0f172a:38bdf8`) | — | Maps pixel luminance across a two-colour shadow-to-highlight gradient. |
| `invert` (`negative`) | `true`, `false`, `1`, `0` | `false` | Inverts RGB channels (`255 - c`) while preserving alpha. |
| `posterize` | `2..=16` | — | Quantises each colour channel into $N$ discrete tonal levels. |
| `threshold` | `0..=255` | — | High-contrast binary 1-bit black/white luminance threshold. |
| `pixelate` | `2..=64` | — | Downsamples pixel blocks of size $N \times N$ pixels. |
| `blur` | `0.5..=20.0` | — | Gaussian blur sigma. |
| `sharpen` | `0.5..=10.0` | — | Unsharp-mask sharpening sigma. |
| `vignette` | `0..=100` | `0` | Radial edge darkening strength. |

#### Policy-controlled watermarks

Watermarks reference logical names declared in `[watermarks]` in `murklet.toml` (for example `brand = "./assets/logo.png"`). Arbitrary file paths or remote watermark URLs are never accepted in query strings.

| Parameter (Aliases) | Values / Range | Default | Description |
| --- | --- | --- | --- |
| `wm` (`watermark`) | `<name>` declared in `[watermarks]` | per-origin `default_watermark` or none | Logical watermark identifier to composite over the image. |
| `wm_pos` | Any `gravity` anchor (`se`, `center`, `nw`, `fp:0.5:0.5`, etc.) | `se` (`south-east`) | Watermark placement (non-centred edge anchors include a 2% margin). |
| `wm_opacity` | `1..=100` | `50` | Watermark alpha opacity percentage (`100` = fully opaque). |
| `wm_scale` | `5..=100` | `20` | Target watermark width as a percentage of the output image's width. |

### Transformation examples

```bash
# 1. Resize a local image to 800px wide WebP at quality 82 (height scaled proportionally)
curl -sS "http://127.0.0.1:9876/load?url=products/shoe.jpg&w=800&q=82&f=webp" -o shoe.webp

# 2. Smart focal-point cover crop (400x400 @ 2x DPR -> 800x800 AVIF) centred on a face at (35%, 20%)
curl -sS "http://127.0.0.1:9876/load?url=portraits/ada.jpg&w=400&h=400&dpr=2&fit=cover&gravity=fp:0.35:0.2&f=avif" -o ada.avif

# 3. Fetch from an S3 origin, apply a duotone palette, subtle vignette, and brand watermark
curl -sS "http://127.0.0.1:9876/load?url=s3://media/hero.png&w=1200&duotone=0f172a:38bdf8&vignette=35&wm=brand&wm_pos=se&wm_opacity=60&wm_scale=15&f=webp" -o hero.webp

# 4. Use a named preset with an explicit width override (shares cache key with equivalent explicit query)
curl -sS "http://127.0.0.1:9876/load?url=products/shoe.jpg&preset=thumb&w=400" -o thumb.webp

# 5. Flatten a transparent PNG onto a light grey background and encode as JPEG
curl -sS "http://127.0.0.1:9876/load?url=logos/mark.png&w=600&bg=f4f4f5&f=jpeg&q=90" -o mark.jpg
```

## Architecture

```text
Client / CDN
     |
HTTP request (GET /load)
     |
Signature check + origin routing + per-origin policy
     |
Preset expansion + Accept negotiation + canonical variant key
     |
Variant cache lookup ------ hit ------> HTTP response (200 / 304)
     |
    miss (coalesced per variant key)
     |
Raw-source cache / coalesced origin fetch (Local / HTTP(S) / S3)
     |
Header guard -> decode -> crop/resize -> filters -> watermark -> encode
     |
Atomic cache write
     |
HTTP response
```

The diagram illustrates conceptual responsibilities rather than mandatory network hops. Conditional requests and cache hits bypass origin fetching and image processing altogether.

Design principles: clear boundaries between transport, origins, security policy, image processing, and caching (see [ADR 0002](docs/adr/0002-single-crate-module-boundaries.md)); explicit domain types and failure modes; bounded concurrency; minimal dependencies; and a replaceable processing engine without speculative abstraction.

## Technology stack

- **Runtime:** Rust (edition 2024), Tokio, and Axum/Hyper.
- **Configuration & CLI:** Serde and TOML (`murklet.example.toml`, strict `deny_unknown_fields`, `--check`, `explain`, `test-policy`, and `sign`).
- **Image engine:** Pure-Rust stack (`image` + SIMD `fast_image_resize` + `ravif`/`rav1e`) behind a replaceable engine boundary (see [ADR 0001](docs/adr/0001-pure-rust-static-image-engine.md)).
- **Telemetry:** `tracing` (`pretty` / `json` logs), lock-free Prometheus `/metrics`, and planned optional OpenTelemetry integration.
- **Testing:** Unit and integration tests, declarative policy test suites (`murklet test-policy`), Criterion benchmarks (`task bench`), and HTTP smoke tests (`task smoke`).

Rust is an implementation choice, not a substitute for architectural discipline or scrutiny of dependencies.

## Roadmap and milestones

| Milestone | Status | Focus |
| --- | --- | --- |
| **0.1 — Image proxy core** | Released | Local and SSRF-hardened HTTP(S) origins, core resize/quality/format pipeline (JPEG/PNG/WebP/GIF), HMAC signing, sharded filesystem cache with request coalescing, and HTTP caching (`ETag`, `304`, `Cache-Control`). |
| **0.2 — AVIF & developer workflow** | Released | Pure-Rust AVIF encoding, `Accept` header negotiation (`AVIF > WebP > JPEG > PNG`), and Taskfile workflow automation. |
| **0.3 — Presets, observability & SIMD** | Released | Structured `pretty`/`json` logs, Prometheus `/metrics`, declarative `[presets.*]`, `murklet explain` CLI, SIMD `fast_image_resize`, and Criterion benchmarks. |
| **0.4 — S3, geometry, filters & eviction** | Released | Pure-Rust S3-compatible (`SigV4`) origin, fit/crop/gravity/focal-point/rotation/DPR/alpha flattening, post-resize colour & monochrome filters, and cache TTL + disk-budget eviction sweeps. |
| **0.5 — Creative filters, watermarks & hardening** | Released | Creative filters (`duotone`, `posterize`, `threshold`, `pixelate`, `vignette`), startup-loaded `[watermarks]`, raw-source cache (`[cache.source]`), transform queue backpressure (`503 Retry-After`), and `murklet sign`. |
| **0.6 — Policy-first control & cache invalidation** | Released | Per-origin policies (`[origins.<kind>.policy]` with `presets_only`), source-bucketed $O(1)$ selective cache purge (`POST /cache/purge`), and declarative CI policy test runner (`murklet test-policy`) per [ADR 0003](docs/adr/0003-policy-first-differentiation-and-per-origin-boundaries.md). |
| **0.7+ — Interoperability & telemetry** | Planned | Optional path-based URL routes, OpenTelemetry trace export, migration ergonomics for established proxy URL formats, and comparative benchmarks. |

## Non-goals

Murklet is not a digital asset manager, browser image editor, image generation service, generic API gateway, mandatory distributed platform, or governance dashboard disguised as an image proxy.

## Quickstart

Requires Rust 1.85+ (edition 2024). If you have [Task](https://taskfile.dev/) installed, common development workflows are available in [Taskfile.yml](Taskfile.yml):

```bash
# Run the full CI gate (format, Clippy, tests)
task check

# Start Murklet locally with optimised release build (recommended for AVIF)
task dev:release

# In another terminal, run the HTTP smoke test (healthz, MISS, HIT, 304)
task smoke

# Inspect how Murklet evaluates a request, origin policy, and canonical cache key
cargo run -- explain "/load?url=sample.png&w=400&f=webp" murklet.example.toml

# Run a declarative TOML policy test suite offline (CI verification)
cargo run -- test-policy policy-tests.toml murklet.toml

# Generate a signed /load URL when server.signing_secret is configured
cargo run -- sign "/load?url=sample.png&w=400&f=webp" murklet.toml

# Run reproducible image-engine benchmarks
task bench
```

Or run Cargo directly:

```bash
# 1. Prepare a configuration file and sample assets directory
cp murklet.example.toml murklet.toml
mkdir -p assets

# 2. Validate configuration without starting the server
cargo run -- --check murklet.toml

# 3. Start Murklet (binds 127.0.0.1:9876 by default)
cargo run -- murklet.toml
```

Operational endpoints:

```bash
# Health check and Prometheus metrics
curl http://127.0.0.1:9876/healthz
curl http://127.0.0.1:9876/metrics

# Selectively purge all cached variants (and raw source cache) for a single source image
curl -X POST -H "X-Cache-Cleanup-Token: <token>" \
  "http://127.0.0.1:9876/cache/purge?url=sample.png"

# Purge the entire cache (when server.cleanup_token is configured)
curl -X POST -H "X-Cache-Cleanup-Token: <token>" http://127.0.0.1:9876/cache/cleanup
```

To build a minimal container image:

```bash
docker build -t murklet:0.6.0 .
```

## Documentation

- [Engineering and Git Conventions](docs/conventions.md) — coding standards, Git Flow, SemVer, and commit guidelines.
- [Architecture Decision Records](docs/adr/README.md) — lightweight record of key technical decisions ([ADR 0001](docs/adr/0001-pure-rust-static-image-engine.md), [ADR 0002](docs/adr/0002-single-crate-module-boundaries.md), [ADR 0003](docs/adr/0003-policy-first-differentiation-and-per-origin-boundaries.md)).
- [Example Configuration](murklet.example.toml) — annotated TOML policy reference.
- [Changelog](CHANGELOG.md) — release history.

## Licence

Licence to be selected before the first public release.
