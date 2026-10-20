//! Frame-cost benchmarks, run on demand:
//!
//! ```text
//! cargo test --release --lib renderer::bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each sample clears the target, records the frame, submits and waits for
//! the GPU, so numbers are serialized CPU+GPU cost per presented frame, not
//! application FPS and not GPU-only time. No window, swapchain or vsync is
//! involved. The first group reproduces the readiness audit's clean-frame
//! workload (N layers, each one 100x100 rectangle with a clip) so the
//! numbers can be compared with the audit's table.

use std::time::Instant;

use super::test_support::Scene;
use super::{Alignment, CachePolicy, ClipShape, Color, Font, FontParameters, Layer, Rounding};

fn median_p95(mut samples: Vec<f64>) -> (f64, f64) {
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    (
        (samples[(n - 1) / 2] + samples[n / 2]) * 0.5,
        samples[n * 95 / 100],
    )
}

fn measure(scene: &mut Scene, warmup: usize, samples: usize) -> (f64, f64) {
    for _ in 0..warmup {
        scene.frame(1.0);
    }
    let times = (0..samples)
        .map(|_| {
            let start = Instant::now();
            scene.frame(1.0);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    median_p95(times)
}

fn audit_scene(
    size: (u32, u32),
    layers: usize,
    policy: CachePolicy,
) -> Option<(Scene, Vec<Layer>)> {
    let mut scene = Scene::new(size)?;
    let layers: Vec<Layer> = (0..layers).map(|_| scene.layer()).collect();
    for (index, layer) in layers.iter().enumerate() {
        let x = (index * 30) as f32;
        layer.set_cache_policy(policy);
        layer
            .set_clip(Some(ClipShape::Rectangle {
                top_left: (x, 0.0),
                size: (100.0, 100.0),
                rounding: Rounding::NONE,
            }))
            .unwrap();
        layer.draw_rectangle(
            (x, 0.0),
            (100.0, 100.0),
            Color::rgb(100, 100, 100),
            Rounding::NONE,
        );
    }
    Some((scene, layers))
}

#[test]
#[ignore = "benchmark; run on demand"]
fn clean_frames_like_the_audit() {
    println!("\nclean frames, N layers each one 100x100 rect + clip (median / p95 ms)");
    println!(
        "{:<14}{:>7}{:>12}{:>22}",
        "target", "layers", "policy", "median / p95"
    );
    for (size, layers) in [
        ((1920, 1080), 1),
        ((1920, 1080), 12),
        ((1920, 1080), 24),
        ((3840, 2160), 12),
        ((3840, 2160), 24),
    ] {
        for (policy, name) in [
            (CachePolicy::Auto, "direct"),
            (CachePolicy::Always, "cached"),
        ] {
            let Some((mut scene, _layers)) = audit_scene(size, layers, policy) else {
                println!("no GPU adapter; skipping");
                return;
            };
            let (median, p95) = measure(&mut scene, 10, 60);
            let stats = scene.stats();
            println!(
                "{:<14}{:>7}{:>12}{:>12.3} / {:<8.3}  {:.1} MiB",
                format!("{}x{}", size.0, size.1),
                layers,
                name,
                median,
                p95,
                stats.bytes.total() as f64 / (1024.0 * 1024.0)
            );
        }
    }
}

/// The design's top risk: a mostly static full-screen text layer with a
/// small layer that changes every frame (a blinking caret). Direct layers
/// replay the text each presented frame; a cached layer is one quad.
#[test]
#[ignore = "benchmark; run on demand"]
fn blinking_caret_over_a_full_screen_text_layer() {
    println!("\ncaret layer redrawn every frame over a static text page (median / p95 ms)");
    for size in [(1920, 1080), (3840, 2160)] {
        for (policy, name) in [
            (CachePolicy::Auto, "direct"),
            (CachePolicy::Always, "cached"),
        ] {
            let Some(mut scene) = Scene::new(size) else {
                println!("no GPU adapter; skipping");
                return;
            };
            let page = scene.layer();
            page.set_cache_policy(policy);
            for line in 0..(size.1 / 24) {
                page.draw_text(
                    "The quick brown fox jumps over the lazy dog, again and again and again.",
                    (16.0, line as f32 * 24.0),
                    Color::rgb(220, 220, 220),
                    Alignment::default(),
                    Font::Named("sans-serif".into()),
                    FontParameters::new(16.0),
                );
            }
            let caret = scene.layer();
            // Warm up: shape and prepare the page once.
            scene.frame(1.0);
            let mut times = Vec::new();
            for frame in 0..80 {
                caret.clear();
                caret.draw_rectangle(
                    (100.0 + frame as f32, 100.0),
                    (2.0, 20.0),
                    Color::rgb(255, 255, 255),
                    Rounding::NONE,
                );
                let start = Instant::now();
                scene.frame(1.0);
                if frame >= 10 {
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            let (median, p95) = median_p95(times);
            let stats = scene.stats();
            println!(
                "{:<12}{:>8}{:>10.3} / {:<8.3} batches={} {:.1} MiB",
                format!("{}x{}", size.0, size.1),
                name,
                median,
                p95,
                stats.text_batches,
                stats.bytes.total() as f64 / (1024.0 * 1024.0)
            );
        }
    }
}
