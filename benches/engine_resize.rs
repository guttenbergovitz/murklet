//! Reproducible Criterion benchmarks for Murklet's pure-Rust image engine.
//!
//! Benchmarks are compiled as a separate target and use explicit error handling
//! without touching production lint rules.

#![allow(missing_docs, clippy::expect_used, clippy::unwrap_used)]

use std::io::Cursor;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use murklet::domain::{
    Axis, DEFAULT_MAX_DIMENSION, Dimension, OutputFormat, Quality, ResizeFilter, TargetDimensions,
    TransformSpec,
};
use murklet::engine::{EngineLimits, transform};

fn make_jpeg_fixture(width: u32, height: u32) -> Vec<u8> {
    let img = RgbImage::from_fn(width, height, |x, y| {
        let r = u8::try_from((x * 7 + y * 3) % 256).unwrap();
        let g = u8::try_from((x * 5 + y * 11) % 256).unwrap();
        let b = u8::try_from((x * 13 + y * 17) % 256).unwrap();
        Rgb([r, g, b])
    });
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Jpeg)
        .unwrap();
    bytes
}

fn bench_engine_transforms(c: &mut Criterion) {
    let source = make_jpeg_fixture(1600, 900);
    let limits = EngineLimits::default();
    let target_800 = TargetDimensions::new(
        Some(Dimension::new(Axis::Width, 800, DEFAULT_MAX_DIMENSION).unwrap()),
        None,
    );

    let mut group = c.benchmark_group("engine_1600x900_to_800w");

    let jpeg_lanczos = TransformSpec {
        dimensions: target_800,
        format: OutputFormat::Jpeg,
        quality: Quality::new(80).unwrap(),
        filter: Some(ResizeFilter::Lanczos),
    };
    group.bench_function("jpeg_lanczos", |b| {
        b.iter(|| transform(black_box(&source), black_box(&jpeg_lanczos), &limits).unwrap());
    });

    let jpeg_bilinear = TransformSpec {
        filter: Some(ResizeFilter::Linear),
        ..jpeg_lanczos
    };
    group.bench_function("jpeg_linear", |b| {
        b.iter(|| transform(black_box(&source), black_box(&jpeg_bilinear), &limits).unwrap());
    });

    let webp_lanczos = TransformSpec {
        format: OutputFormat::Webp,
        ..jpeg_lanczos
    };
    group.bench_function("webp_lanczos", |b| {
        b.iter(|| transform(black_box(&source), black_box(&webp_lanczos), &limits).unwrap());
    });

    group.finish();
}

criterion_group!(benches, bench_engine_transforms);
criterion_main!(benches);
