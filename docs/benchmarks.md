# Empirical Benchmarks and Resource Sizing

This document records reproducible measurements of Murklet (`v0.9.5`, `--release`) under concurrent transformation pressure, overload backpressure, and filesystem cache eviction. All numbers are baseline single-host measurements on Apple Silicon macOS (local NVMe SSD) and are intended to verify resource-isolation invariants and guide operator capacity planning rather than serve as marketing claims.

## 1. Live process pressure test (RSS, CPU, latency, and backpressure)

### Test setup

- **Binary:** `target/release/murklet` (`v0.9.5`).
- **Dataset:** 24 distinct textured `1600×1200` PNG assets (~5.5 MiB each on disk, 1.92 MP / 7.7 MiB decoded RGBA per frame).
- **Limits (`[limits]`):**
  - `max_concurrent_transforms = 4`
  - `max_concurrent_fetches = 16`
  - `max_pending_transforms = 12`
  - `max_in_flight_source_bytes = 64 MiB` (`67108864`)
  - `transform_queue_timeout_secs = 2`
- **Sampling:** OS process RSS and CPU% sampled via `ps -o rss=,%cpu=` every 50 ms throughout each workload; Prometheus `/metrics` scraped immediately after each run.
- **Baseline idle RSS:** `4.6 MiB`.

### Results

| Workload | Requests / concurrency | `200` / `503` | p50 (ms) | p95 (ms) | p99 (ms) | Peak RSS | Peak CPU |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **JPEG** (`1600×1200 → 800w`, `q=80`) | 24 distinct / 12 | `24` / `0` | 57.4 | 79.7 | 80.2 | **110.6 MiB** | 99.7% |
| **WebP** (`1600×1200 → 800w`, `q=80`) | 24 distinct / 12 | `24` / `0` | 45.1 | 62.2 | 65.9 | **128.5 MiB** | 132.7% |
| **PNG** (`1600×1200 → 800w`) | 16 distinct / 8 | `16` / `0` | 30.0 | 42.2 | 42.2 | **128.8 MiB** | 173.3% |
| **AVIF overload** (`1600×1200 → 640w`, `q=65`) | 24 distinct / 24 (`pending=12`) | `12` / `12` | 1437.7 | 2187.7 | 2190.4 | **157.2 MiB** | **401.6%** |
| **Warm cache HIT** (8 variants) | 200 / 32 | `200` / `0` | 5.2 | 10.2 | 11.9 | **157.2 MiB** | 273.5% |

### Observed invariants and sizing guidance

1. **CPU isolation matches `max_concurrent_transforms`:** Under 24 concurrent AVIF requests with `max_concurrent_transforms = 4` and `max_pending_transforms = 12`, peak CPU capped at `401.6%` (~4 cores), exactly 12 excess requests shed immediately with `503 Retry-After` (`murklet_backpressure_rejections_total{reason="pending_queue"} 12`), and the remaining 12 completed with `200 OK`.
2. **Process RSS vs `max_in_flight_source_bytes`:** With a `64 MiB` raw source byte budget, peak process RSS reached `~111 MiB` for JPEG, `~129 MiB` for WebP/PNG, and `~157 MiB` for 4 concurrent AVIF (`rav1e`) encoders. As a rule of thumb, provision container memory for **`2.0–2.5×` `max_in_flight_source_bytes`** plus decoded pixel headroom (`max_concurrent_transforms × max_decoded_pixels × 4 bytes`) and allocator high-water-mark retention.
3. **Zero resource leaks:** After every workload, all in-flight Prometheus gauges (`murklet_in_flight_transforms`, `murklet_in_flight_fetches`, `murklet_in_flight_source_bytes`) returned to `0`.

## 2. Filesystem cache eviction (`benches/cache_evict.rs`)

Measured via `cargo bench --bench cache_evict -- --quick`:

| Benchmark | Median time |
| --- | --- |
| `fs_cache_evict/scan_within_budget/1000` (1,000 files) | **31.7 ms** |
| `fs_cache_evict/scan_within_budget/10000` (10,000 files) | **219.9 ms** |
| `fs_cache_evict/sort_and_evict_10pct/1000` (evict oldest 10% of 1,000) | **130.8 ms** |

Directory scanning scales roughly linearly (~22 ms per 1,000 cached files on local NVMe). For typical single-node deployments holding tens of thousands of variants, periodic background sweeps (`sweep_interval_secs = 300`) remain inexpensive without introducing an external metadata database. Multi-million-file caches would warrant an indexed eviction store in a future milestone.

## Reproducing locally

```bash
# Image-engine and cache-eviction Criterion benchmarks
task bench

# Deterministic stampede, backpressure, cancellation, and purge stress suite
task stress
```
