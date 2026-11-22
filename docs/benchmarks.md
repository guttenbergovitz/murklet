# Empirical Benchmarks and Resource Sizing

This document records reproducible measurements of Murklet (`v0.9.5`/`v0.9.6`, `--release`) under concurrent transformation pressure, overload backpressure, high-megapixel filter/watermark pipelines, and filesystem cache eviction. All numbers are baseline single-host measurements on Apple Silicon macOS (local NVMe SSD) and are intended to verify behaviour under specific workloads and guide operator capacity planning rather than serve as universal worst-case guarantees.

## 1. Live process pressure test — 1.92 MP assets (RSS, CPU, latency, and backpressure)

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

## 2. Heavy 12 MP pipeline (large decoded frames + filters + watermark + AVIF)

To measure decoded-pixel, filter, and encoder memory when compressed inputs are small on disk relative to decoded frame size, 8 distinct `4000×3000` (12 MP, ~48 MiB decoded RGBA each, ~368 KiB compressed PNG on disk) assets were processed concurrently at `max_concurrent_transforms = 4` with `max_in_flight_source_bytes = 64 MiB`:

| Workload | Requests / concurrency | `200` / `503` | p50 (ms) | p95 (ms) | p99 (ms) | Peak RSS | Peak CPU |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **12 MP → 1600w JPEG** (`duotone` + `blur=1.5` + `vignette=30` + `wm=brand`) | 8 distinct / 4 | `8` / `0` | 148.7 | 177.9 | 178.4 | **269.3 MiB** | 218.5% |
| **12 MP → 1200w AVIF** (`blur=1.5` + `vignette=30` + `wm=brand`, `q=65`) | 8 distinct / 4 | `8` / `0` | 1054.4 | 1083.0 | 1090.3 | **375.0 MiB** | **400.7%** |

### Observed behaviour and sizing guidance

1. **Observed CPU behaviour under tested workloads:** In both AVIF tests at `max_concurrent_transforms = 4`, peak CPU stayed near `~401%` (~4 cores) and excess requests beyond `max_pending_transforms = 12` shed immediately with `503 Retry-After` (`murklet_backpressure_rejections_total{reason="pending_queue"} 12`). This confirms that the transform semaphore bounds active blocking tasks for the tested pure-Rust encoders, though it is not a hard OS-level CPU quota (e.g. cgroups).
2. **Decoded pixels and encoder contexts dominate RSS on large images:** On the 12 MP workload, raw in-flight source payloads totalled under `2 MiB`, yet peak RSS reached **`269.3 MiB`** (JPEG + filters + watermark) and **`375.0 MiB`** (AVIF + filters + watermark). Do **not** size container memory as a simple multiple of `max_in_flight_source_bytes`. Instead, budget both components independently:
   - raw source queue (`max_in_flight_source_bytes`), **plus**
   - concurrent transform working set (`max_concurrent_transforms × (max_decoded_pixels × 4 bytes + filter/resize intermediates + encoder context — notably ~30–40 MiB per concurrent AVIF encoder)`), **plus**
   - allocator high-water-mark retention (system allocators such as macOS `malloc` / glibc `ptmalloc` typically retain peak arenas rather than returning them immediately to the OS).
3. **Zero resource leaks:** After every workload, all in-flight Prometheus gauges (`murklet_in_flight_transforms`, `murklet_in_flight_fetches`, `murklet_in_flight_source_bytes`) returned to `0`.

## 3. Filesystem cache eviction (`benches/cache_evict.rs`)

Measured via `cargo bench --bench cache_evict -- --quick`:

| Benchmark | Median time |
| --- | --- |
| `fs_cache_evict/scan_within_budget/1000` (1,000 files) | **31.7 ms** |
| `fs_cache_evict/scan_within_budget/10000` (10,000 files) | **219.9 ms** |
| `fs_cache_evict/sort_and_evict_10pct/1000` (evict oldest 10% of 1,000) | **130.8 ms** |

Directory scanning scales roughly linearly (~22 ms per 1,000 cached files on local NVMe). For typical single-node deployments holding tens of thousands of variants, periodic background sweeps (`sweep_interval_secs = 300`) remain inexpensive without introducing an external metadata database. Multi-million-file caches would warrant an indexed eviction store in a future milestone.

## Reproducing locally

```bash
# Criterion benchmarks: image engine and cache eviction
task bench

# Live release build: RSS, CPU, p50/p95/p99, and backpressure (sections 1 and 2 above)
task bench:live                     # 1.92 MP standard profile
task bench:live PROFILE=heavy       # 12 MP + filters + watermark profile

# Deterministic stampede, backpressure, cancellation, and purge stress suite
task stress
```

The live harness is [scripts/bench/live_pressure.py](../scripts/bench/live_pressure.py). It needs only Python 3 and a release binary (`MURKLET_BIN` overrides the default `target/release/murklet`), generates its own PNG fixtures, and samples RSS and CPU through `ps`. Absolute numbers depend on hardware, operating system, and allocator, so compare runs on the same machine.

> [!NOTE]
> The tables above were recorded with an earlier ad-hoc fixture generator. The committed harness builds more repetitive (more compressible) images, so its RSS and latency figures come out lower (for example roughly 95 MiB peak for the standard AVIF overload case instead of 157 MiB), while the qualitative behaviour is the same: CPU capped near four cores, exactly 12 requests shed with `reason="pending_queue"`, and all gauges back at zero. Treat the tables as indicative samples and re-measure with your own images.
