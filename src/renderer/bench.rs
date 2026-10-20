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
use super::{
    Alignment, CachePolicy, ClipShape, Color, Font, FontParameters, Layer, Rounding, TextSpan,
};

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

/// A UI-like layer that interleaves shapes and short labels (many text
/// batches) and changes its text every frame, as a live readout does.
#[test]
#[ignore = "benchmark; run on demand"]
fn interleaved_labels_changing_every_frame() {
    println!("\nN (rect + label) pairs, one label changes each frame (median / p95 ms)");
    for pairs in [4usize, 16, 64] {
        let Some(mut scene) = Scene::new((1920, 1080)) else {
            println!("no GPU adapter; skipping");
            return;
        };
        let layer = scene.layer();
        let mut times = Vec::new();
        for frame in 0..80 {
            layer.clear();
            for i in 0..pairs {
                let y = i as f32 * 14.0;
                layer.draw_rectangle(
                    (0.0, y),
                    (300.0, 12.0),
                    Color::rgb(40, 40, 40),
                    Rounding::NONE,
                );
                let label = if i == 0 {
                    format!("frame {frame}")
                } else {
                    format!("row {i}")
                };
                layer.draw_text(
                    &label,
                    (4.0, y),
                    Color::rgb(220, 220, 220),
                    Alignment::default(),
                    Font::Named("sans-serif".into()),
                    FontParameters::new(10.0),
                );
            }
            let start = Instant::now();
            scene.frame(1.0);
            if frame >= 10 {
                times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let (median, p95) = median_p95(times);
        println!(
            "{pairs:>4} pairs  {median:>8.3} / {p95:<8.3}  text_batches={}",
            scene.stats().text_batches
        );
    }
}

/// An editor viewport: per line a background, a syntax-highlighted line of
/// four spans and a selection stripe on a few of them, with the cursor line
/// edited every frame. Compared with merging off, so the numbers show what
/// batch merging buys (and costs) on the same scene.
#[test]
#[ignore = "benchmark; run on demand"]
fn editor_viewport_with_and_without_merging() {
    println!("\neditor viewport, 1920x1080 (median / p95 ms)");
    for (lines, scrolling) in [(30usize, false), (120, false), (30, true), (120, true)] {
        println!(
            "-- {lines} lines, {}",
            if scrolling {
                "every line changes each frame (scrolling)"
            } else {
                "one line edited per frame"
            }
        );
        for merge in [false, true] {
            let Some(mut scene) = Scene::new((1920, 1080)) else {
                println!("no GPU adapter; skipping");
                return;
            };
            let layer = scene.layer();
            layer.0.borrow_mut().set_merge_batches_for_test(merge);
            let font = Font::Named("monospace".into());
            let params = FontParameters::new(14.0);
            let colors = [
                Color::rgb(200, 120, 220),
                Color::rgb(230, 230, 230),
                Color::rgb(120, 200, 140),
                Color::rgb(240, 200, 120),
            ];
            let mut times = Vec::new();
            for frame in 0..80 {
                layer.clear();
                for line in 0..lines {
                    let y = line as f32 * 18.0;
                    layer.draw_rectangle(
                        (0.0, y),
                        (1920.0, 18.0),
                        if line == 5 {
                            Color::rgb(45, 45, 60)
                        } else {
                            Color::rgb(30, 30, 36)
                        },
                        Rounding::NONE,
                    );
                    if line % 7 == 3 {
                        layer.draw_rectangle(
                            (60.0, y),
                            (220.0, 18.0),
                            Color::rgba(80, 120, 200, 120),
                            Rounding::NONE,
                        );
                    }
                    let tail = if line == 5 || scrolling {
                        format!("edit {frame}")
                    } else {
                        format!("{line}")
                    };
                    let spans: Vec<TextSpan> = ["fn ", "render(", &tail, ") {}"]
                        .iter()
                        .zip(colors.iter().cloned())
                        .map(|(text, color)| TextSpan::new(*text, font.clone(), params, color))
                        .collect();
                    layer.draw_styled_text(&spans, (8.0, y + 1.0), Alignment::default());
                }
                layer.draw_rectangle(
                    (100.0, 5.0 * 18.0),
                    (2.0, 18.0),
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
            let ops = layer.0.borrow().op_kinds_for_test().len();
            println!(
                "   merge={merge:<5}  {median:>8.3} / {p95:<8.3}  ops={ops:<4} text_batches={}",
                scene.stats().text_batches
            );
        }
    }
}
