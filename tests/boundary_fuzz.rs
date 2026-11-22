//! Deterministic fuzz-style and adversarial boundary tests covering Murklet's
//! untrusted-input parsers and guards: HTTP query parsing, `Accept` negotiation,
//! SSRF URL/IP validation, and image header/decoder bomb resilience.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use murklet::cache::CacheKey;
use murklet::domain::{DEFAULT_MAX_DIMENSION, TransformSpec};
use murklet::engine::{EngineError, EngineLimits, transform};
use murklet::http::{RawLoadQuery, negotiate_format, parse_load_query};
use murklet::origin::{HostAllowlist, is_forbidden_ip, validate_remote_url};

/// Tiny deterministic PRNG (xorshift64*) so fuzz corpora are 100% reproducible in CI.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        &slice[self.next_usize(slice.len())]
    }

    fn next_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(u8::try_from(self.next_u64() & 0xFF).unwrap());
        }
        out
    }
}

#[test]
fn fuzz_query_parser_never_panics_on_adversarial_and_mutated_inputs() {
    let weird_strings = [
        "",
        "0",
        "-1",
        "1",
        "99999999999999999999",
        "NaN",
        "inf",
        "-inf",
        "1e309",
        "0.0",
        "-0.0",
        "fp:0.5:0.5",
        "fp:NaN:0.5",
        "fp:-1:2",
        "fp:0.5",
        "fp:0.5:0.5:0.5",
        "#fff",
        "000000:ffffff",
        "zzzzzz:111111",
        ":::",
        "../evil",
        "brand\0null",
        "webp",
        "avif",
        "svg",
        "true",
        "false",
        "hv",
        "90",
        "180",
        "270",
        "360",
        "🦀🔥",
    ];

    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_BABE);
    for _ in 0..600 {
        let maybe = |r: &mut Rng| -> Option<String> {
            if r.next_usize(3) == 0 {
                None
            } else {
                Some((*r.pick(&weird_strings)).to_owned())
            }
        };

        let raw = RawLoadQuery {
            url: maybe(&mut rng),
            preset: maybe(&mut rng),
            w: maybe(&mut rng),
            h: maybe(&mut rng),
            dpr: maybe(&mut rng),
            fit: maybe(&mut rng),
            gravity: maybe(&mut rng),
            rotate: maybe(&mut rng),
            bg: maybe(&mut rng),
            q: maybe(&mut rng),
            f: maybe(&mut rng),
            filter: maybe(&mut rng),
            flip: maybe(&mut rng),
            brightness: maybe(&mut rng),
            contrast: maybe(&mut rng),
            saturation: maybe(&mut rng),
            hue: maybe(&mut rng),
            gamma: maybe(&mut rng),
            grayscale: maybe(&mut rng),
            sepia: maybe(&mut rng),
            tint: maybe(&mut rng),
            duotone: maybe(&mut rng),
            invert: maybe(&mut rng),
            posterize: maybe(&mut rng),
            threshold: maybe(&mut rng),
            pixelate: maybe(&mut rng),
            blur: maybe(&mut rng),
            sharpen: maybe(&mut rng),
            vignette: maybe(&mut rng),
            wm: maybe(&mut rng),
            wm_pos: maybe(&mut rng),
            wm_opacity: maybe(&mut rng),
            wm_scale: maybe(&mut rng),
        };

        if let Ok(parsed) = parse_load_query(raw, DEFAULT_MAX_DIMENSION) {
            // Every accepted query must yield a non-empty source and valid canonical cache key.
            assert!(!parsed.source.is_empty());
            let key = CacheKey::from_parts("local", &parsed.source, &parsed.spec);
            assert_eq!(key.digest_hex().len(), 64);
        }
    }
}

#[test]
fn fuzz_ssrf_url_validator_and_ip_classifier_reject_adversarial_bypass_attempts() {
    let allowlist = HostAllowlist::new(&[
        "cdn.example.com".to_owned(),
        "*.assets.example.org".to_owned(),
    ]);

    let must_reject = [
        "",
        "file:///etc/passwd",
        "gopher://127.0.0.1:6379/_INFO",
        "ftp://cdn.example.com/a.jpg",
        "http://127.0.0.1/a.jpg",
        "http://[::1]/a.jpg",
        "http://[::ffff:127.0.0.1]/a.jpg",
        "http://[::ffff:169.254.169.254]/latest/meta-data/",
        "http://[64:ff9b::169.254.169.254]/latest/meta-data/",
        "http://169.254.169.254/latest/meta-data/",
        "http://100.64.0.1/internal.png",
        "http://2130706433/a.jpg",
        "http://0x7f000001/a.jpg",
        "https://user:pass@cdn.example.com/a.jpg",
        "https://cdn.example.com@127.0.0.1/a.jpg",
        "https://evil-cdn.example.com/a.jpg",
        "https://cdn.example.com.evil.com/a.jpg",
        "https://assets.example.org/a.jpg",
        "https://cdn.example.com\\@127.0.0.1/a.jpg",
        "http://localhost/a.jpg",
        "https://cdn.example.com\r\nHost: 127.0.0.1/a.jpg",
    ];

    for url in must_reject {
        assert!(
            validate_remote_url(url, &allowlist, true).is_err(),
            "SSRF validator unexpectedly accepted adversarial URL: {url:?}"
        );
    }

    // Verify key forbidden IP ranges directly.
    let forbidden_ips = [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001)),
        IpAddr::V6(Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0xa9fe, 0xa9fe)),
    ];
    for ip in forbidden_ips {
        assert!(
            is_forbidden_ip(ip),
            "expected IP {ip} to be classified as forbidden"
        );
    }

    // Mutated URLs never panic.
    let schemes = ["http://", "https://", "file://", "ftp://", ""];
    let hosts = [
        "cdn.example.com",
        "img.assets.example.org",
        "127.0.0.1",
        "[::1]",
        "evil.test",
        "user@cdn.example.com",
    ];
    let paths = ["/", "/a.png", "/../etc/passwd", "/a?b=1#c", "\0", ""];
    let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
    for _ in 0..400 {
        let candidate = format!(
            "{}{}{}",
            rng.pick(&schemes),
            rng.pick(&hosts),
            rng.pick(&paths)
        );
        let _ = validate_remote_url(&candidate, &allowlist, true);
    }
}

#[test]
fn fuzz_image_engine_rejects_corrupted_headers_random_bytes_and_decompression_bombs() {
    let limits = EngineLimits::default();
    let spec = TransformSpec::default();

    // 1. Crafted PNG header advertising 65535x65535 dimensions (decompression bomb)
    // must be rejected at header inspection before full pixel allocation.
    let mut bomb_png = Vec::new();
    bomb_png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    bomb_png.extend_from_slice(&13u32.to_be_bytes());
    bomb_png.extend_from_slice(b"IHDR");
    bomb_png.extend_from_slice(&65_535u32.to_be_bytes()); // width
    bomb_png.extend_from_slice(&65_535u32.to_be_bytes()); // height
    bomb_png.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, RGBA, compression, filter, interlace
    bomb_png.extend_from_slice(&0xb605_d950u32.to_be_bytes()); // valid CRC-32 for this IHDR chunk
    bomb_png.extend_from_slice(&0u32.to_be_bytes());
    bomb_png.extend_from_slice(b"IDAT");
    bomb_png.extend_from_slice(&0x35af_061eu32.to_be_bytes()); // standard empty IDAT CRC-32
    bomb_png.extend_from_slice(&0u32.to_be_bytes());
    bomb_png.extend_from_slice(b"IEND");
    bomb_png.extend_from_slice(&0xae42_6082u32.to_be_bytes()); // standard IEND CRC-32
    let bomb_err = transform(&bomb_png, &spec, &limits).unwrap_err();
    assert!(
        matches!(
            bomb_err,
            EngineError::SourceDimensionLimitExceeded { .. }
                | EngineError::DecodedPixelLimitExceeded { .. }
        ),
        "expected header guard rejection for 65535x65535 PNG, got {bomb_err:?}"
    );

    // 2. Crafted GIF header advertising 65535x65535 dimensions.
    let mut bomb_gif = b"GIF89a".to_vec();
    bomb_gif.extend_from_slice(&65_535u16.to_le_bytes());
    bomb_gif.extend_from_slice(&65_535u16.to_le_bytes());
    bomb_gif.extend_from_slice(&[0x00, 0x00, 0x00]); // packed, bg, aspect
    bomb_gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x00, 0x02, 0x00, 0x3B]);
    let gif_err = transform(&bomb_gif, &spec, &limits).unwrap_err();
    assert!(
        matches!(
            gif_err,
            EngineError::SourceDimensionLimitExceeded { .. }
                | EngineError::DecodedPixelLimitExceeded { .. }
        ),
        "expected header guard rejection for 65535x65535 GIF, got {gif_err:?}"
    );

    // 3. Truncated valid PNG and JPEG slices never panic.
    let tiny = RgbImage::from_pixel(8, 8, Rgb([100, 150, 200]));
    for format in [ImageFormat::Png, ImageFormat::Jpeg] {
        let mut encoded = Vec::new();
        DynamicImage::ImageRgb8(tiny.clone())
            .write_to(&mut Cursor::new(&mut encoded), format)
            .unwrap();
        for cut in 0..encoded.len() {
            let _ = transform(&encoded[..cut], &spec, &limits);
        }
    }

    // 4. Random byte payloads never panic and are cleanly rejected.
    let mut rng = Rng::new(0xFEED_FACE_1122_3344);
    for _ in 0..300 {
        let len = rng.next_usize(512);
        let payload = rng.next_bytes(len);
        assert!(transform(&payload, &spec, &limits).is_err());
    }
}

#[test]
fn fuzz_accept_negotiation_never_panics_on_arbitrary_headers() {
    let headers = [
        "",
        "*/*",
        "image/avif,image/webp,image/*,*/*;q=0.8",
        "image/avif;q=0, image/webp;q=0, image/jpeg;q=0",
        "image/avif;q=NaN, image/webp;q=-5, image/png;q=999",
        ";;;;;;q=1, , , /",
        "image/avif;a=b;c=d;q=0.5;e=f",
        "🦀/🔥;q=0.9",
    ];
    for header in headers {
        let _ = negotiate_format(Some(header));
    }
}
