# Changelog

All notable changes to Murklet are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

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
