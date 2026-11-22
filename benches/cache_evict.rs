//! Reproducible Criterion benchmarks for [`FsCache::evict`] directory traversal
//! and oldest-`mtime` disk-budget eviction across sharded source-bucketed trees.
//!
//! Run default scales (1k and 10k entries):
//! `cargo bench --bench cache_evict`
//!
//! Include the 100k-entry scale (takes longer to populate temporary fixtures):
//! `MURKLET_BENCH_LARGE=1 cargo bench --bench cache_evict`

#![allow(missing_docs, clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use murklet::cache::FsCache;

static NEXT_BENCH_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TempBenchDir {
    path: PathBuf,
}

impl TempBenchDir {
    fn new(label: &str) -> Self {
        let id = NEXT_BENCH_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("murklet-bench-evict-{label}-{pid}-{id}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempBenchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Populates a realistic 256-shard, source-bucketed cache tree with `count` entries
/// (`<2-hex-shard>/<source-digest>/<variant>.jpg`).
fn populate_sharded_cache(root: &Path, count: usize, payload_bytes: usize) {
    let payload = vec![0xABu8; payload_bytes];
    // Spread entries across 256 hex shards and ~4 variants per source directory.
    let variants_per_source = 4usize;

    for idx in 0..count {
        let source_id = idx / variants_per_source;
        let variant_id = idx % variants_per_source;
        let shard = format!("{:02x}", source_id % 256);
        let source_dir = root.join(&shard).join(format!("{source_id:064x}"));
        if variant_id == 0 {
            fs::create_dir_all(&source_dir).unwrap();
        }
        let file_path = source_dir.join(format!("v{variant_id:016x}.jpg"));
        fs::write(&file_path, &payload).unwrap();
    }
}

fn bench_cache_evict(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_cache_evict");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(6));

    let payload_bytes = 64usize;
    let mut scales: Vec<usize> = vec![1_000, 10_000];
    if std::env::var("MURKLET_BENCH_LARGE").as_deref() == Ok("1") {
        scales.push(100_000);
    }

    // 1. Scan-only sweep (total bytes within budget, no sorting or deletions).
    for &count in &scales {
        let dir = TempBenchDir::new(&format!("scan-{count}"));
        populate_sharded_cache(dir.path(), count, payload_bytes);
        let generous_budget = u64::try_from(count * payload_bytes * 2).unwrap();
        let cache = FsCache::new(dir.path(), 1024 * 1024)
            .unwrap()
            .with_max_total_bytes(Some(generous_budget))
            .with_ttl(Some(Duration::from_secs(86_400)));

        group.bench_with_input(
            BenchmarkId::new("scan_within_budget", count),
            &count,
            |b, _| {
                b.iter(|| {
                    let report = cache.evict().unwrap();
                    black_box(report);
                });
            },
        );
    }

    // 2. Over-budget sweep (full scan + O(n log n) mtime sort + evicting oldest 10%).
    let evict_count = 1_000usize;
    let tight_budget = u64::try_from((evict_count * 9 / 10) * payload_bytes).unwrap();
    group.bench_function(BenchmarkId::new("sort_and_evict_10pct", evict_count), |b| {
        b.iter_batched(
            || {
                let dir = TempBenchDir::new("evict-1k");
                populate_sharded_cache(dir.path(), evict_count, payload_bytes);
                let cache = FsCache::new(dir.path(), 1024 * 1024)
                    .unwrap()
                    .with_max_total_bytes(Some(tight_budget));
                (dir, cache)
            },
            |(_dir, cache)| {
                let report = cache.evict().unwrap();
                black_box(report);
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_cache_evict);
criterion_main!(benches);
