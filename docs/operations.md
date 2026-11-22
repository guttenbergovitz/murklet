# Operations guide

Practical notes for running Murklet: what the metrics mean, which limit to tune when `503` responses appear, how to size memory, and what the cache invalidation contract guarantees. Design rationale lives in the [ADRs](adr/README.md); measured numbers live in [benchmarks.md](benchmarks.md).

## Running in a container

Pre-built multi-arch images (`linux/amd64` and `linux/arm64`) are published to Docker Hub on every `v*.*.*` release tag:

```bash
docker run --rm -p 9876:9876 \
  -v "$PWD/assets:/app/assets:ro" \
  -v murklet-cache:/app/cache \
  guttenbergovitz/murklet:latest
```

Pin a specific release tag (for example `guttenbergovitz/murklet:0.9.7`) in production. To build locally instead:

```bash
docker build -t murklet:local .          # or: task docker:build
docker run --rm -p 9876:9876 \
  -v "$PWD/assets:/app/assets:ro" \
  -v murklet-cache:/app/cache \
  murklet:local
```

The image runs as a non-root user (`uid 10001`) and listens on `0.0.0.0:9876` using the bundled `murklet.example.toml`. To use your own configuration, mount it over `/app/murklet.toml` and keep `server.bind` reachable from outside the container (`0.0.0.0:9876`). Validate any configuration offline first with `murklet --check <file>`.

### Publishing releases to Docker Hub (maintainers)

`.github/workflows/docker-publish.yml` builds and pushes `linux/amd64` and `linux/arm64` images (with BuildKit provenance and SBOM attestations) whenever a `v*.*.*` tag is pushed. Configure two repository secrets in GitHub Settings → Secrets and variables → Actions:

- `DOCKERHUB_USERNAME` — Docker Hub account username (`guttenbergovitz`).
- `DOCKERHUB_TOKEN` — Docker Hub Personal Access Token with **Read & Write** scope.

## Reading `503 Retry-After`

Every `503` caused by resource limits carries `Retry-After` and increments `murklet_backpressure_rejections_total{reason="..."}`. The label tells you which limit to look at:

| `reason` | Meaning | Limit | Typical response |
| --- | --- | --- | --- |
| `pending_queue` | Cache-miss admission was full; shed immediately before touching any origin. | `max_pending_transforms` | Expected under bursts of distinct uncached URLs. Raise only if CPU and memory have headroom; otherwise put a CDN in front. |
| `fetch_timeout` | Waited too long for an origin fetch slot. | `max_concurrent_fetches` | Slow or saturated origin. Check origin latency before raising the limit. |
| `byte_budget` | Raw source payloads in flight would exceed the budget (including source-cache hits). | `max_in_flight_source_bytes` | Large sources arriving concurrently. Raise the budget together with container memory. |
| `transform_timeout` | Waited too long for a CPU transform slot. | `max_concurrent_transforms`, `transform_queue_timeout_secs` | CPU-bound workload (AVIF is the heaviest). Scale out or reduce output cost. |

Related gauges (`murklet_in_flight_transforms`, `murklet_in_flight_fetches`, `murklet_in_flight_source_bytes`) must return to `0` when the service is idle. A gauge that stays above zero while idle is a bug worth reporting.

## Memory sizing

`max_in_flight_source_bytes` bounds **raw source payloads only**. It is not a process RSS limit. Budget memory from three independent parts:

1. The raw source queue: `max_in_flight_source_bytes`.
2. The transform working set: `max_concurrent_transforms × (decoded pixels × 4 bytes + resize/filter intermediates + encoder context)`. AVIF encoder contexts are the largest contributor.
3. Allocator high-water-mark retention: system allocators usually keep peak arenas rather than returning them to the operating system promptly.

[benchmarks.md](benchmarks.md) shows, for example, a peak of roughly 375 MiB RSS for four concurrent 12 MP AVIF transforms with filters and a watermark, while raw sources stayed under 2 MiB. Measure your own image mix before choosing limits; the figures there are single-host samples, not worst-case guarantees.

## Cache behaviour

- **Variant cache** (`[cache]`) and optional **raw-source cache** (`[cache.source]`) live on the local filesystem and are protected by TTL (`ttl_secs`), a disk budget (`max_total_bytes`), and a periodic sweep (`sweep_interval_secs`).
- **Purge contract:** `POST /cache/purge?url=<source>` deletes cached variants and the raw source entry, and guarantees that no request *started before the purge* can write a stale entry back to disk. A request already in flight may still finish and return its response. `POST /cache/cleanup` gives the same guarantee for the whole cache. Both require `X-Cache-Cleanup-Token` and fail closed with `401` when `server.cleanup_token` is unset.
- **Single instance only:** invalidation epochs are held in memory. Several instances sharing one networked cache directory would need external coordination or versioned cache keys (see [ADR 0004](adr/0004-resource-isolation-and-invalidation-semantics.md)).
- **Eviction cost:** the sweep scans the cache directory. Measured cost is roughly linear (about 220 ms for 10,000 files on local NVMe); see [benchmarks.md](benchmarks.md).

## Metrics reference

Exposed at `GET /metrics` (Prometheus text format; `404` when `server.metrics_enabled = false`):

| Metric | Type | Description |
| --- | --- | --- |
| `murklet_load_requests_total` | counter | `GET /load` requests by cache outcome and status class. |
| `murklet_transforms_total` | counter | Completed transformations by origin and output format. |
| `murklet_in_flight_transforms` | gauge | CPU-bound transformations currently executing. |
| `murklet_in_flight_fetches` | gauge | Origin fetches currently executing. |
| `murklet_in_flight_source_bytes` | gauge | Raw source bytes currently held in memory. |
| `murklet_transform_duration_seconds` | histogram | Decode, transform, and encode time only; excludes fetch and queueing. |
| `murklet_cache_cleanups_total` | counter | Successful `POST /cache/cleanup` runs. |
| `murklet_cache_errors_total` | counter | Cache read/write errors by tier and operation (requests degrade to origin rather than fail). |
| `murklet_backpressure_rejections_total` | counter | `503` rejections by resource-limit reason (see above). |
