//! Reproducible pressure and concurrency stress tests verifying Murklet's
//! behaviour under stampedes, resource-limit saturation, and concurrent cache purges.
//!
//! Run directly via `cargo test --test pressure_stress --locked` or `task stress`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::header::ETAG;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use murklet::cache::FsCache;
use murklet::http::{AppState, build_router};
use murklet::origin::LocalFsOrigin;
use tokio::sync::Barrier;
use tower::ServiceExt;

static NEXT_STRESS_ID: AtomicU64 = AtomicU64::new(0);

struct StressFixture {
    root: PathBuf,
    origin_dir: PathBuf,
    cache_dir: PathBuf,
}

impl StressFixture {
    fn new(label: &str) -> Self {
        let id = NEXT_STRESS_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("murklet-stress-{label}-{pid}-{id}"));
        let origin_dir = root.join("origin");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&origin_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        Self {
            root,
            origin_dir,
            cache_dir,
        }
    }

    fn write_png(&self, name: &str, width: u32, height: u32, colour: [u8; 4]) {
        let img = RgbaImage::from_fn(width, height, |x, y| {
            let r = colour[0].wrapping_add(u8::try_from(x % 31).unwrap());
            let g = colour[1].wrapping_add(u8::try_from(y % 31).unwrap());
            Rgba([r, g, colour[2], colour[3]])
        });
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        fs::write(self.origin_dir.join(name), bytes).unwrap();
    }

    fn state(&self) -> AppState {
        let local = LocalFsOrigin::new(&self.origin_dir, 10 * 1024 * 1024).unwrap();
        let cache = FsCache::new(&self.cache_dir, 10 * 1024 * 1024).unwrap();
        AppState::new(local, cache)
    }
}

impl Drop for StressFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn scrape_metrics(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_stampede_coalesces_64_concurrent_requests_into_single_transform() {
    let fixture = StressFixture::new("stampede");
    fixture.write_png("hero.png", 320, 200, [40, 120, 200, 255]);

    let app = build_router(Arc::new(fixture.state()));
    let concurrency = 64usize;
    let barrier = Arc::new(Barrier::new(concurrency));

    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let app = app.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/load?url=hero.png&w=120&f=webp")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let etag = resp
                .headers()
                .get(ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (status, etag, body.len())
        }));
    }

    let mut first_etag: Option<String> = None;
    for handle in handles {
        let (status, etag, len) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(len > 0);
        if let Some(ref expected) = first_etag {
            assert_eq!(&etag, expected);
        } else {
            first_etag = Some(etag);
        }
    }

    let metrics = scrape_metrics(&app).await;
    assert!(
        metrics.contains("murklet_transforms_total{origin=\"local\",format=\"webp\"} 1\n"),
        "expected exactly 1 transform under 64x stampede, got:\n{metrics}"
    );
    assert!(metrics.contains("murklet_in_flight_transforms 0\n"));
    assert!(metrics.contains("murklet_in_flight_fetches 0\n"));
    assert!(metrics.contains("murklet_in_flight_source_bytes 0\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_and_memory_ceiling_shed_load_predictably_under_distinct_burst() {
    let fixture = StressFixture::new("burst");
    let total_requests = 32usize;
    for idx in 0..total_requests {
        fixture.write_png(
            &format!("img-{idx:02}.png"),
            400,
            300,
            [u8::try_from((idx * 7) % 256).unwrap(), 90, 150, 255],
        );
    }

    let mut state = fixture
        .state()
        .with_max_concurrent_transforms(2)
        .with_max_concurrent_fetches(4)
        .with_max_pending_transforms(4)
        .with_max_in_flight_source_bytes(256 * 1024);
    state.transform_queue_timeout = Duration::from_millis(80);
    let app = build_router(Arc::new(state));

    let barrier = Arc::new(Barrier::new(total_requests));
    let mut handles = Vec::with_capacity(total_requests);
    for idx in 0..total_requests {
        let app = app.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/load?url=img-{idx:02}.png&w=160&f=jpeg"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            (status, retry_after)
        }));
    }

    let mut ok_count = 0usize;
    let mut rejected_count = 0usize;
    for handle in handles {
        let (status, retry_after) = handle.await.unwrap();
        match status {
            StatusCode::OK => ok_count += 1,
            StatusCode::SERVICE_UNAVAILABLE => {
                assert_eq!(retry_after.as_deref(), Some("2"));
                rejected_count += 1;
            }
            other => panic!("unexpected status under burst load: {other}"),
        }
    }

    assert!(ok_count > 0, "expected some admitted requests to succeed");
    assert!(
        rejected_count > 0,
        "expected tight pending/memory limits to shed excess burst load with 503"
    );

    let metrics = scrape_metrics(&app).await;
    assert!(
        metrics.contains("murklet_backpressure_rejections_total{"),
        "expected backpressure rejection counter in metrics:\n{metrics}"
    );
    assert!(metrics.contains("murklet_in_flight_transforms 0\n"));
    assert!(metrics.contains("murklet_in_flight_fetches 0\n"));
    assert!(metrics.contains("murklet_in_flight_source_bytes 0\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuous_purge_under_concurrent_load_never_resurrects_stale_variant() {
    let fixture = StressFixture::new("purge-load");
    fixture.write_png("live.png", 160, 120, [10, 20, 200, 255]);

    let mut state = fixture.state();
    state.cleanup_token = Some("stress-token".to_owned());
    let app = build_router(Arc::new(state));

    // Capture initial stale ETag.
    let initial = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/load?url=live.png&w=64&f=jpeg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(initial.status(), StatusCode::OK);
    let initial_etag = initial
        .headers()
        .get(ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    // Run 12 concurrent readers while mutating + purging the source 4 times.
    let mut readers = Vec::new();
    for _ in 0..12 {
        let app = app.clone();
        readers.push(tokio::spawn(async move {
            for _ in 0..6 {
                let resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri("/load?url=live.png&w=64&f=jpeg")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
            }
        }));
    }

    for generation in 1u8..=4 {
        fixture.write_png(
            "live.png",
            160,
            120,
            [generation * 50, 200 - generation * 30, 40, 255],
        );
        let purge = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/purge?url=live.png")
                    .header("x-cache-cleanup-token", "stress-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(purge.status(), StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    for reader in readers {
        reader.await.unwrap();
    }

    // Write final distinct source + purge; next read must MISS and serve the latest ETag.
    fixture.write_png("live.png", 160, 120, [240, 220, 10, 255]);
    let final_purge = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/cache/purge?url=live.png")
                .header("x-cache-cleanup-token", "stress-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(final_purge.status(), StatusCode::OK);

    let after_purge = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/load?url=live.png&w=64&f=jpeg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after_purge.status(), StatusCode::OK);
    assert_eq!(after_purge.headers().get("x-cache").unwrap(), "MISS");
    let final_etag = after_purge.headers().get(ETAG).unwrap().to_str().unwrap();
    assert_ne!(final_etag, initial_etag);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_cancellation_and_overload_leaks_zero_permits_or_gauges() {
    let fixture = StressFixture::new("cancel-overload");
    for idx in 0..12 {
        fixture.write_png(
            &format!("asset-{idx:02}.png"),
            200,
            150,
            [u8::try_from((idx * 19) % 256).unwrap(), 110, 180, 255],
        );
    }

    let mut state = fixture
        .state()
        .with_max_concurrent_transforms(2)
        .with_max_concurrent_fetches(4)
        .with_max_pending_transforms(6)
        .with_max_in_flight_source_bytes(256 * 1024);
    state.transform_queue_timeout = Duration::from_millis(200);
    let app = build_router(Arc::new(state));

    let mut tasks = Vec::new();
    for idx in 0..12 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            app.oneshot(
                Request::builder()
                    .uri(format!("/load?url=asset-{idx:02}.png&w=96&f=webp"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        }));
    }

    // Abort half of the requests mid-flight to simulate abrupt client disconnects.
    tokio::time::sleep(Duration::from_millis(5)).await;
    for (idx, handle) in tasks.iter().enumerate() {
        if idx % 2 == 0 {
            handle.abort();
        }
    }

    for (idx, handle) in tasks.into_iter().enumerate() {
        if idx % 2 == 0 {
            let _ = handle.await;
        } else {
            let resp = handle.await.unwrap().unwrap();
            assert!(
                matches!(
                    resp.status(),
                    StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                ),
                "unexpected status: {}",
                resp.status()
            );
        }
    }

    // Wait briefly for any detached `spawn_blocking` tasks from aborted requests to finish.
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let m = scrape_metrics(&app).await;
            if m.contains("murklet_in_flight_transforms 0\n")
                && m.contains("murklet_in_flight_fetches 0\n")
                && m.contains("murklet_in_flight_source_bytes 0\n")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "in-flight gauges failed to return to 0 after mixed cancellations:\n{}",
        scrape_metrics(&app).await
    );
}
