# Murklet

Images under control.

Murklet is an open-source image proxy and on-demand image transformation service, written in Rust.

It fetches images from trusted sources, transforms and optimises them, caches the results, and serves them efficiently over HTTP. Its emphasis is straightforward: dependable image delivery first, backed by explicit policies, predictable caching, and practical observability.

> **Project status:** `v0.4.0` (early release). Implemented and tested: local filesystem, SSRF-hardened HTTP(S), and pure-Rust S3-compatible (`SigV4`) origins; pure-Rust static image engine with SIMD `fast_image_resize` (JPEG, PNG, WebP, GIF, and AVIF), fit/cover/contain/scale-down cropping with gravity & focal points, rotation, DPR, alpha flattening, post-resize colour/tonal/monochrome & creative filters (`duotone`, `posterize`, `threshold`, `pixelate`, `vignette`), and policy-controlled watermarks (`[watermarks]`); sharded filesystem variant & optional raw-source cache with request coalescing, TTL expiry, and disk-budget eviction; transform queue backpressure (`503 Retry-After`); declarative TOML configuration with named presets, `murklet explain`, and `murklet sign`; optional HMAC-SHA256 request signing; `Accept` format negotiation; structured `tracing` logs; Prometheus `/metrics`; and Criterion benchmarks. OpenTelemetry exporters remain planned.

## Motivation

Murklet exists for two equally candid reasons:

1. **Hands-on engineering in Rust:** It is a deliberate vehicle for working directly with lower-level Rust concepts — ownership and zero-copy boundaries, bounded concurrency, async I/O, FFI trade-offs, and resource isolation — on a real networked service rather than toy exercises.
2. **Real operational friction in existing proxies:** Mature image proxies already exist, yet operating them in practice still exposes recurring pain points: sprawling environment-variable configuration that is hard to audit or test, weak separation of policies per origin, and the need to bolt on an external caching tier even for modest deployments.

Murklet is not trying to differentiate itself merely by being written in Rust, supporting AVIF, or publishing synthetic benchmarks. The aim is a small, dependable image proxy whose behaviour is easy to control, explain, test, and operate:

- **Image proxy first:** Fetching, resizing, cropping, encoding, caching, and HTTP delivery are the core product.
- **Multiple origins:** Image sources are adapters behind a narrow contract, not assumptions baked into the HTTP or processing layers.
- **Secure by default:** Trusted origins, signed requests where appropriate, and bounded use of CPU, memory, network, and storage.
- **Predictable caching:** Canonical transformation specifications and versioned cache identities.
- **Observable decisions:** Useful metrics, traces, and structured logs without mandating a heavy observability stack.
- **Simple deployment:** A single service should be useful without a database, Kubernetes, or a control plane.

## Planned capabilities

### Image sources

Murklet is not tied to S3. The origin interface should support different source types without altering the image-processing pipeline.

| Source | Intended role |
| --- | --- |
| HTTP(S) | Fetch images from explicitly trusted remote hosts, including existing websites and CMS/PIM endpoints. |
| Local filesystem | Serve assets from configured directories with path-traversal protection. |
| S3-compatible object storage | AWS S3 and compatible providers (such as MinIO or Cloudflare R2), subject to integration testing. |
| Google Cloud Storage | Potential dedicated adapter if native authentication or provider behaviour warrants it. |
| Azure Blob Storage | Potential dedicated adapter if demand justifies it. |
| Custom origins | Future adapters behind an explicit, narrow origin contract. |

Initial target: HTTP(S), local filesystem, and S3-compatible storage. Public arbitrary-URL fetching will not be enabled by default.

### Transformations

- On-demand resize, crop, fit/fill, and rotation.
- Quality and compression controls.
- JPEG, PNG, WebP, GIF, and AVIF (pure-Rust encoding).
- Format negotiation via the HTTP `Accept` header (with explicit `Vary` and CDN cache-key awareness).
- EXIF orientation handling, sensible defaults, and configurable limits.
- Stable, documented URL semantics suitable for use behind a CDN.

### Cache and delivery

- Cache both source objects (where appropriate) and transformed variants.
- Canonical transformation parameters and versioned cache keys.
- Filesystem-backed cache initially, with additional storage backends only when justified.
- `ETag`, conditional requests, and explicit `Cache-Control` behaviour.
- Coalescing of concurrent requests for the same variant to avoid cache stampedes.
- A defined approach to freshness, invalidation, and upstream origin changes.

### Security and reliability

- Origin allowlists and optional request signatures (for example, HMAC).
- SSRF defences across redirects, DNS resolution, and network routing.
- Limits on input bytes, image dimensions, decoded pixels, processing time, memory, and concurrent work.
- Protection against decompression bombs and unsafe file paths.
- Backpressure, timeouts, and explicit error handling.
- Explicit consideration of native decoder and library risks, even when the HTTP server is written in Rust.

### Policies and observability

Policies should remain optional for simple setups and expressive for production use: which origins are allowed, which transformations are permitted, and what resource limits apply. Policies should be declarative, validated, and testable.

Operations should be transparent through structured logs, Prometheus-compatible metrics, and optional OpenTelemetry traces. High-cardinality labels and noisy per-request instrumentation will be avoided by default.

## Architecture direction

```text
Client / CDN
     |
HTTP request
     |
Validation + origin/transform policy
     |
Canonical variant identity
     |
Cache lookup ------ hit ------> HTTP response
     |
    miss
     |
Origin adapter (HTTP / filesystem / S3-compatible / ...)
     |
Fetch + resource limits
     |
Decode -> transform -> encode
     |
Cache write
     |
HTTP response
```

The diagram illustrates conceptual responsibilities rather than mandatory network hops. Conditional requests and cache hits should bypass unnecessary work.

Design principles: clear boundaries between transport, origins, security policy, image processing, and caching; explicit domain types and failure modes; bounded concurrency; minimal dependencies; and a replaceable processing engine without speculative abstraction.

## Technology direction

- **Runtime:** Rust (edition 2024), Tokio, and Axum/Hyper.
- **Configuration:** Serde and TOML (`murklet.example.toml`, strict `deny_unknown_fields`, `--check`, and `explain`).
- **Image engine:** Pure-Rust stack (`image` + SIMD `fast_image_resize` + `ravif`/`rav1e`) behind a replaceable engine boundary (see [ADR 0001](docs/adr/0001-pure-rust-static-image-engine.md)).
- **Telemetry:** `tracing` (`pretty` / `json` logs), lock-free Prometheus `/metrics`, and optional future OpenTelemetry integration.
- **Testing:** Unit and integration tests, Criterion benchmarks (`task bench`), and planned fuzzing and golden-image tests.

Rust is an implementation choice, not a substitute for architectural discipline or scrutiny of native dependencies.

## Proposed milestones

| Milestone | Focus |
| --- | --- |
| **0.1 — Image proxy core** | Core origins (HTTP(S), filesystem, and S3-compatible), common transformations, JPEG/WebP, secure fetching, signed requests, filesystem cache, HTTP caching, and a minimal runnable deployment. |
| **0.2 — Controlled delivery** | Declarative, testable policies; stable/versioned cache identities; operational metrics and tracing. |
| **0.3 — Broader interoperability** | AVIF and additional origin adapters based on demand; compatibility/migration tooling for established proxy URL formats; comparative benchmarks. |

These are planning targets, not release commitments.

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

# Inspect how Murklet evaluates a request and computes its canonical cache key
cargo run -- explain "/load?url=sample.png&w=400&f=webp" murklet.example.toml

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

Example requests:

```bash
# Health check and Prometheus metrics
curl http://127.0.0.1:9876/healthz
curl http://127.0.0.1:9876/metrics

# Resize and convert a local asset to WebP
curl -i "http://127.0.0.1:9876/load?url=sample.png&w=800&q=80&f=webp" -o out.webp

# Purge cache (when server.cleanup_token is configured)
curl -X POST -H "X-Cache-Cleanup-Token: <token>" http://127.0.0.1:9876/cache/cleanup
```

To build a minimal container image:

```bash
docker build -t murklet:0.4.0 .
```

## Documentation

- [Engineering and Git Conventions](docs/conventions.md) — coding standards, Git Flow, SemVer, and commit guidelines.
- [Architecture Decision Records](docs/adr/README.md) — lightweight record of key technical decisions.
- [Example Configuration](murklet.example.toml) — annotated TOML policy reference.
- [Changelog](CHANGELOG.md) — release history.

## Licence

Licence to be selected before the first public release.
