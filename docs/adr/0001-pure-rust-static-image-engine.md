# 0001. Pure-Rust image engine with static-first processing

## Status

Accepted

## Context

Murklet requires an image-processing engine for v0.1 capable of decoding, resizing, and encoding common web formats under strict memory, pixel, and latency limits. Two main paths were considered:

1. **`libvips` via FFI:** Mature, fast, streaming memory profile, and built-in multi-frame animation handling, but introduces native C build dependencies, harder cross-compilation, FFI safety boundaries, and upstream C decoder CVE exposure.
2. **Pure-Rust image stack:** Builds into a single static binary with straightforward tooling, memory-safe decoding and resampling, and direct alignment with Murklet's goal of hands-on Rust systems engineering.

In parallel, supporting multi-frame animated GIF and WebP resizing in v0.1 adds frame-disposal composition complexity and significant CPU/memory amplification risk (a small animated file can expand into hundreds of full-resolution frames).

## Decision

- Use a **pure-Rust image-processing stack** for v0.1 behind a narrow internal engine boundary so the implementation remains replaceable without affecting HTTP, policy, origin, or cache modules.
- Target **static images** in v0.1 and defer multi-frame animated resizing for future evaluation.
- Handle animated inputs with an **explicit fallback policy**: transform the first frame by default, or allow controlled passthrough of the original bytes when permitted by policy and size limits.

## Consequences

**Easier:**
- Single-binary builds, simple CI and container images, and no system `libvips` dependency.
- Predictable memory-safety boundary and straightforward resource limits (`max_input_bytes`, `max_decoded_pixels`).
- Simpler v0.1 pipeline (`DecodedImage -> Transform -> EncodedImage`).

**Harder / trade-offs accepted:**
- Higher peak memory usage on very large static images compared with `libvips`'s demand-driven streaming pipeline; mitigated by strict input-byte and decoded-pixel limits.
- Animated GIF/WebP inputs will not preserve animation when resized in v0.1.
- Lossy WebP or AVIF encoding performance and compression quality in pure Rust will need benchmarking as those encoders are added; if pure-Rust encoders prove insufficient later, a separate ADR will evaluate alternatives.
