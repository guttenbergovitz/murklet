# 0004. Resource isolation and linearisable cache invalidation

## Status

Accepted

## Context

In `v0.6.0`, Murklet bounded CPU-intensive image decoding and encoding with a single `max_concurrent_transforms` semaphore acquired *after* fetching the raw source payload. Under high concurrency across distinct uncached URLs, this allowed unbounded origin fetches and unbounded raw source buffers to queue in memory while waiting for transform slots.

Separately, `POST /cache/purge?url=<source>` and `POST /cache/cleanup` deleted files from disk without synchronising against in-flight cache-miss requests. A request that had already fetched stale bytes could finish transforming after a purge returned `200 OK` and write the stale variant back onto disk.

## Decision

1. **Four-stage resource isolation (`[limits]`):** Split backpressure across four independent, observable guards enforced in `src/application/load.rs`:
   - **Pending admission (`max_pending_transforms`):** Non-blocking `try_acquire` before touching any origin so excess cache-miss load sheds immediately with `503 Retry-After`.
   - **Origin fetch concurrency (`max_concurrent_fetches`):** Bounded semaphore held only for the duration of the origin read (local, HTTP(S), or S3).
   - **In-flight source memory budget (`max_in_flight_source_bytes`):** KiB-granularity semaphore permits (`ByteBudget` / `ByteReservation`) reserved *before* buffer pre-allocation (`metadata.len()` / `Content-Length`) and incrementally *before* appending each streamed chunk, updating `murklet_in_flight_source_bytes` live and releasing immediately after transformation.
   - **Transform concurrency (`max_concurrent_transforms`):** Bounded semaphore protecting CPU and decoded-pixel memory during `spawn_blocking` execution.
   All permits (`pending`, `transform`, `ByteReservation`) and RAII metric guards are moved *into* the `spawn_blocking` closure so client cancellation mid-transform cannot release concurrency or memory slots while CPU-bound encoding is still running.
   Each rejection increments `murklet_backpressure_rejections_total{reason=...}` alongside RAII gauges for in-flight fetches, source bytes, and transforms, while `murklet_transform_duration_seconds` measures CPU transform duration inside `spawn_blocking`.

2. **Linearisable in-memory invalidation epochs (`src/cache/invalidation.rs`):**
   - Each cache-miss acquires an RAII `FlightToken` before reading from origin, recording the global cleanup epoch and per-source generation.
   - `POST /cache/purge` and `POST /cache/cleanup` advance the relevant generation and hold an exclusive `RwLock` I/O gate while deleting files on disk.
   - Variant and raw-source cache writes hold a shared `RwLock` read gate and verify that their `FlightToken` is still current before writing; stale flights skip disk persistence without blocking concurrent writers.
   - **Explicit invalidation contract:** `POST /cache/purge` guarantees that existing cached files are deleted and that no request started before the purge can repopulate stale entries on disk. Callers whose requests were already in flight when the purge ran may still finish receiving their in-progress response rather than forcing an automatic retry.
   - Tracking entries are removed automatically when the last `FlightToken` for a source drops, keeping memory bounded to active flights.

3. **Explicit cache-error observability:** Filesystem cache read/write errors still degrade gracefully to origin rather than failing requests with `500`, but emit `tracing::warn!` logs and increment `murklet_cache_errors_total{cache,op}`.

## Consequences

**Easier:**
- Memory usage, upstream origin concurrency, and CPU transform queues remain bounded under distinct-URL bursts and slow origins.
- Operators can see from `/metrics` exactly which resource limit triggered `503` backpressure and whether disk cache errors are silently increasing origin load.
- `POST /cache/purge` and `POST /cache/cleanup` guarantee that in-flight requests started before invalidation cannot repopulate stale files on disk.

**Harder / trade-offs accepted:**
- In-memory invalidation epochs protect a **single Murklet instance** (Murklet's primary deployment model). Multi-instance deployments sharing a networked cache filesystem would require a shared coordination store or versioned external cache keys.
- `FsCache::evict` still performs an $O(n \log n)$ directory scan during periodic sweeps; `benches/cache_evict.rs` was added to measure real costs before introducing any index complexity.
