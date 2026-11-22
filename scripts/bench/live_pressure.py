#!/usr/bin/env python3
"""Live-process pressure benchmark for Murklet (RSS, CPU, latency, backpressure).

Starts a release build of Murklet against generated PNG fixtures, drives it with
concurrent HTTP load, and samples process RSS/CPU via `ps` (macOS and Linux).
Standard library only. Results are indicative single-host numbers, not guarantees.

Usage:
    cargo build --release --locked
    python3 scripts/bench/live_pressure.py --profile standard
    python3 scripts/bench/live_pressure.py --profile heavy

Environment:
    MURKLET_BIN  Path to the release binary (default: target/release/murklet).
"""

import argparse
import concurrent.futures
import os
import shutil
import statistics
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import zlib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MURKLET_BIN = Path(os.environ.get("MURKLET_BIN", REPO_ROOT / "target/release/murklet"))
PORT = 19876
BASE_URL = f"http://127.0.0.1:{PORT}"
TOKEN = "bench-token"


def make_png(width: int, height: int, seed: int, rgba: bool = False) -> bytes:
    """Builds a deterministic textured PNG so encoders do real work."""
    channels = 4 if rgba else 3
    tile = bytes(
        180 if (rgba and i % 4 == 3) else (i // channels * 13 + seed * 31) & 0xFF
        for i in range(min(width, 256) * channels)
    )
    row_len = width * channels
    row = (tile * (row_len // len(tile) + 1))[:row_len]
    raw = b"".join(
        b"\x00" + bytes((b + y * 7) & 0xFF for b in row) for y in range(height)
    )

    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack("!I", len(data)) + tag + data + struct.pack("!I", crc)

    ihdr = struct.pack("!IIBBBBB", width, height, 8, 6 if rgba else 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 1))
        + chunk(b"IEND", b"")
    )


class ProcessSampler:
    """Samples RSS (KiB) and CPU% of a PID in a background thread."""

    def __init__(self, pid: int, interval: float = 0.05) -> None:
        self.pid, self.interval = pid, interval
        self.stop_event = threading.Event()
        self.rss_kib: list[int] = []
        self.cpu_pct: list[float] = []
        self.thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> tuple[float, float, float]:
        self.stop_event.set()
        self.thread.join(timeout=2.0)
        if not self.rss_kib:
            return 0.0, 0.0, 0.0
        return (
            max(self.rss_kib) / 1024.0,
            statistics.mean(self.rss_kib) / 1024.0,
            max(self.cpu_pct),
        )

    def _run(self) -> None:
        while not self.stop_event.is_set():
            out = subprocess.run(
                ["ps", "-o", "rss=,%cpu=", "-p", str(self.pid)],
                capture_output=True,
                text=True,
                check=False,
            )
            parts = out.stdout.split()
            if len(parts) == 2:
                try:
                    self.rss_kib.append(int(parts[0]))
                    self.cpu_pct.append(float(parts[1]))
                except ValueError:
                    pass
            time.sleep(self.interval)


def http_get(path: str, timeout: float = 60.0) -> tuple[int, float]:
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(f"{BASE_URL}{path}", timeout=timeout) as resp:
            resp.read()
            return resp.status, (time.perf_counter() - started) * 1000.0
    except urllib.error.HTTPError as err:
        err.read()
        return err.code, (time.perf_counter() - started) * 1000.0


def cleanup_cache() -> None:
    request = urllib.request.Request(
        f"{BASE_URL}/cache/cleanup",
        method="POST",
        headers={"X-Cache-Cleanup-Token": TOKEN},
    )
    urllib.request.urlopen(request, timeout=10.0).read()


def scrape_gauges() -> dict[str, str]:
    with urllib.request.urlopen(f"{BASE_URL}/metrics", timeout=5.0) as resp:
        text = resp.read().decode("utf-8")
    prefixes = ("murklet_in_flight_", "murklet_backpressure_rejections_total")
    return dict(line.rsplit(" ", 1) for line in text.splitlines() if line.startswith(prefixes))


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    pos = (len(ordered) - 1) * p / 100.0
    low, high = int(pos), min(int(pos) + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (pos - low)


def run_case(label: str, pid: int, urls: list[str], concurrency: int, clear: bool = True) -> None:
    if clear:
        cleanup_cache()
        time.sleep(0.1)
    sampler = ProcessSampler(pid)
    sampler.start()
    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        results = list(pool.map(http_get, urls))
    wall = time.perf_counter() - started
    peak_rss, avg_rss, peak_cpu = sampler.stop()
    ok = [lat for code, lat in results if code == 200]
    rejected = sum(1 for code, _ in results if code == 503)
    print(f"=== {label} ===")
    print(f"  requests={len(urls)} concurrency={concurrency} wall={wall:.2f}s ok={len(ok)} 503={rejected}")
    print(
        f"  latency_ms(200): p50={percentile(ok, 50):.1f} "
        f"p95={percentile(ok, 95):.1f} p99={percentile(ok, 99):.1f}"
    )
    print(f"  rss_mib: peak={peak_rss:.1f} avg={avg_rss:.1f} | peak_cpu={peak_cpu:.1f}%")
    print(f"  gauges: {scrape_gauges()}")


def write_config(path: Path, assets: Path, cache: Path, watermark: Path, profile: str) -> None:
    heavy = profile == "heavy"
    path.write_text(
        f"""
[server]
bind = "127.0.0.1:{PORT}"
metrics_enabled = true
cleanup_token = "{TOKEN}"

[cache]
dir = "{cache}"
max_entry_bytes = 52428800

[limits]
max_input_bytes = 52428800
max_source_dimension = 16384
max_target_dimension = 8192
max_decoded_pixels = 64000000
max_concurrent_transforms = 4
max_concurrent_fetches = {8 if heavy else 16}
max_pending_transforms = {8 if heavy else 12}
max_in_flight_source_bytes = 67108864
transform_queue_timeout_secs = {10 if heavy else 2}

[origins.local]
enabled = true
root = "{assets}"

[watermarks]
brand = "{watermark}"
""",
        encoding="utf-8",
    )


def standard_cases(pid: int) -> None:
    def distinct(n: int, query: str) -> list[str]:
        return [f"/load?url=photo-{i:02d}.png&{query}" for i in range(n)]

    run_case("JPEG 1.9 MP -> 800w q=80 (conc=12)", pid, distinct(24, "w=800&f=jpeg&q=80"), 12)
    run_case("WebP 1.9 MP -> 800w q=80 (conc=12)", pid, distinct(24, "w=800&f=webp&q=80"), 12)
    run_case("PNG 1.9 MP -> 800w (conc=8)", pid, distinct(16, "w=800&f=png"), 8)
    run_case("AVIF overload 1.9 MP -> 640w (conc=24, pending cap 12)", pid, distinct(24, "w=640&f=avif&q=65"), 24)
    cleanup_cache()
    for url in distinct(8, "w=800&f=jpeg&q=80"):
        http_get(url)
    hits = [f"/load?url=photo-{i % 8:02d}.png&w=800&f=jpeg&q=80" for i in range(200)]
    run_case("Warm cache HITs (200 requests, conc=32)", pid, hits, 32, clear=False)


def heavy_cases(pid: int) -> None:
    jpeg = "w=1600&f=jpeg&q=82&duotone=0f172a:38bdf8&blur=1.5&vignette=30&wm=brand&wm_opacity=60"
    avif = "w=1200&f=avif&q=65&blur=1.5&vignette=30&wm=brand&wm_opacity=60"
    for label, query in (("12 MP JPEG + filters + watermark", jpeg), ("12 MP AVIF + filters + watermark", avif)):
        urls = [f"/load?url=big-{i:02d}.png&{query}" for i in range(8)]
        run_case(f"{label} (8 distinct, conc=4)", pid, urls, 4)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--profile", choices=("standard", "heavy"), default="standard")
    profile = parser.parse_args().profile

    if not MURKLET_BIN.is_file():
        raise SystemExit(f"release binary not found at {MURKLET_BIN}; run `cargo build --release --locked`")

    tmp = Path(tempfile.mkdtemp(prefix="murklet-bench-"))
    proc = None
    try:
        assets, cache = tmp / "assets", tmp / "cache"
        assets.mkdir()
        cache.mkdir()
        watermark = tmp / "brand.png"
        watermark.write_bytes(make_png(256, 96, 99, rgba=True))

        width, height, count, prefix = (4000, 3000, 8, "big") if profile == "heavy" else (1600, 1200, 24, "photo")
        print(f"Generating {count} distinct {width}x{height} PNG fixtures ({profile} profile)...")
        for i in range(count):
            (assets / f"{prefix}-{i:02d}.png").write_bytes(make_png(width, height, i + 1))

        config = tmp / "murklet.toml"
        write_config(config, assets, cache, watermark, profile)
        proc = subprocess.Popen([str(MURKLET_BIN), str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        for _ in range(100):
            try:
                if http_get("/healthz", timeout=1.0)[0] == 200:
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise SystemExit("murklet did not become healthy in time")

        (heavy_cases if profile == "heavy" else standard_cases)(proc.pid)
    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
