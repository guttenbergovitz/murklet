# 0003. Policy-first differentiation and per-origin boundaries

## Status

Accepted

## Context

Most established self-hosted image proxies (such as `imgproxy` and `imgrust`) wrap native `libvips` via FFI, rely heavily on flat environment-variable configuration, and delegate variant caching to external tiers (CDN, Varnish, or S3 write-back).

During early development (`v0.4.0`–`v0.5.0`), Murklet intentionally expanded its pure-Rust image pipeline with creative post-resize filters and policy-controlled watermarks to cover common web delivery needs without requiring native C dependencies or paid tiers. Continuing to chase `libvips` feature parity indefinitely, however, would dilute Murklet's core purpose as stated in `README.md`: predictable, auditable, single-binary image delivery.

## Decision

- Anchor Murklet's roadmap on **operational and policy differentiation** rather than open-ended filter parity:
  1. **Per-origin policies:** Allow each configured origin to enforce its own transformation boundaries (`presets_only`, `allowed_presets`, `allowed_formats`, `max_target_dimension`, `default_watermark`, and `cache_control` overrides) in declarative TOML.
  2. **Self-contained cache control:** Extend the built-in sharded filesystem cache with selective per-source invalidation (`POST /cache/purge`) so operators do not need an external caching proxy for modest deployments.
  3. **Auditable, testable policies:** Keep configuration strict (`deny_unknown_fields`), explainable (`murklet explain`), and verifiable in CI (`murklet test-policy`).
  4. **Interoperability and verification:** Add optional path-based URL routes for CDN/migration ergonomics, OpenTelemetry tracing, and comparative benchmarks against `libvips`-based proxies.
- Treat the current pure-Rust filter and watermark set as complete for the foreseeable future unless a concrete delivery requirement justifies an addition.

## Consequences

**Easier:**
- Clear architectural focus: new work strengthens policy isolation, cache semantics, and operator ergonomics rather than adding speculative image effects.
- Public deployments can lock origins to `presets_only = true` without mandatory URL signing, preventing arbitrary-dimension cache-busting attacks.
- Policy regressions become catchable in CI before deployment.

**Harder / trade-offs accepted:**
- Murklet will not attempt to match every niche image-manipulation operator offered by `libvips`-based proxies.
- Per-origin policies and selective cache invalidation add modest schema and cache-index complexity that must remain strictly validated at startup.
