//! GPU tests for batch merging and wrapped styled text: merging must change
//! how many ops a layer replays and never a pixel of what they draw.

use super::test_support::Scene;
use super::{Alignment, ClipShape, Color, Font, FontParameters, Layer, Pixels, Rounding, TextSpan};

fn font() -> Font {
    Font::Named("sans-serif".into())
}

fn label(layer: &Layer, text: &str, at: (f32, f32)) {
    layer.draw_text(
        text,
        at,
        Color::rgb(255, 255, 255),
        Alignment::default(),
        font(),
        FontParameters::new(12.0),
    );
}

#[test]
fn disjoint_rect_text_lines_merge_to_two_batches() {
    let Some(mut scene) = Scene::new((128, 128)) else {
        return;
    };
    let layer = scene.layer();
    for line in 0..6 {
        let y = line as f32 * 16.0;
        layer.draw_rectangle(
            (0.0, y),
            (128.0, 16.0),
            Color::rgb(40, 40, 90),
            Rounding::NONE,
        );
        label(&layer, "let x = 1;", (4.0, y + 1.0));
    }
    scene.frame(1.0);
    assert_eq!(layer.0.borrow().op_kinds_for_test(), ["solid", "text"]);
    assert_eq!(scene.stats().text_batches, 1);
    // The text is above its line's background, not under it.
    let inked = (0..128).any(|x| (0..16).any(|y| scene.pixel(x, y)[0] > 150));
    assert!(inked, "text is visible over the line background");
}

#[test]
fn overlapping_commands_are_never_reordered() {
    let Some(mut scene) = Scene::new((128, 128)) else {
        return;
    };
    let layer = scene.layer();
    label(&layer, "MMMMMMMM", (0.0, 0.0));
    // Covers the first label entirely...
    layer.draw_rectangle(
        (0.0, 0.0),
        (128.0, 16.0),
        Color::rgb(0, 0, 255),
        Rounding::NONE,
    );
    // ...and a label drawn over the cover must stay above it.
    label(&layer, "MMMMMMMM", (0.0, 0.0));
    scene.frame(1.0);
    assert_eq!(
        layer.0.borrow().op_kinds_for_test(),
        ["text", "solid", "text"]
    );
    let covered = (0..128).any(|x| (0..16).any(|y| scene.pixel(x, y)[0] > 150));
    assert!(covered, "the second label shows over the cover");
}

/// Deterministic pseudo-random numbers, so a failing scene can be replayed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }

    fn unit(&mut self) -> f32 {
        self.below(1000) as f32 / 1000.0
    }
}

fn random_scene(layer: &Layer, seed: u64, clipped: bool) {
    let mut rng = Lcg(seed);
    let words = ["fn main()", "MMM", "let x = 1;", "iiiiiiii", "Wide W"];
    let image = Pixels::new(2, 2, [255, 128, 0, 255].repeat(4));
    let mut open_clips = 0;
    for _ in 0..40 {
        let x = rng.below(110) as f32;
        let y = rng.below(110) as f32;
        let color = Color::rgba(
            rng.below(256) as u8,
            rng.below(256) as u8,
            rng.below(256) as u8,
            128 + rng.below(128) as u8,
        );
        match rng.below(if clipped { 8 } else { 6 }) {
            0 | 1 => layer.draw_rectangle(
                (x, y),
                (4.0 + rng.below(40) as f32, 4.0 + rng.below(20) as f32),
                color,
                if rng.below(3) == 0 {
                    Rounding::uniform(3.0)
                } else {
                    Rounding::NONE
                },
            ),
            2 => layer.draw_circle((x, y), 3.0 + rng.below(12) as f32, color),
            3 => label(layer, words[rng.below(5) as usize], (x, y)),
            4 => layer.draw_styled_text(
                &[
                    TextSpan::new(
                        "ab",
                        font(),
                        FontParameters::new(11.0),
                        Color::rgb(255, 200, 0),
                    ),
                    TextSpan::new(
                        "cd",
                        font(),
                        FontParameters::new(15.0),
                        Color::rgb(0, 200, 255),
                    ),
                ],
                (x, y),
                Alignment::default(),
            ),
            5 => layer.draw_image(
                image.clone(),
                (x, y),
                (6.0 + rng.unit() * 20.0, 6.0 + rng.unit() * 20.0),
                Color::rgb(255, 255, 255),
            ),
            6 if open_clips < 2 => {
                // Clip scopes are barriers; the scene opens them by hand.
                layer
                    .0
                    .borrow_mut()
                    .push_clip_for_test(ClipShape::Rectangle {
                        top_left: (x.floor(), y.floor()),
                        size: (60.0, 50.0),
                        rounding: Rounding::NONE,
                    });
                open_clips += 1;
            }
            _ if open_clips > 0 => {
                layer.0.borrow_mut().pop_clip_for_test();
                open_clips -= 1;
            }
            _ => {}
        }
    }
    for _ in 0..open_clips {
        layer.0.borrow_mut().pop_clip_for_test();
    }
}

#[test]
fn merging_never_changes_a_pixel() {
    for clipped in [false, true] {
        for seed in 1..=24u64 {
            let Some(mut scene) = Scene::new((128, 128)) else {
                return;
            };
            let merged = scene.layer();
            random_scene(&merged, seed, clipped);
            scene.frame(1.0);
            let with_merging = scene.pixels();
            let merged_ops = merged.0.borrow().op_kinds_for_test().len();

            let plain = scene.layer();
            plain.0.borrow_mut().set_merge_batches_for_test(false);
            random_scene(&plain, seed, clipped);
            merged.clear();
            scene.frame(1.0);
            let without_merging = scene.pixels();
            let plain_ops = plain.0.borrow().op_kinds_for_test().len();

            assert!(
                with_merging == without_merging,
                "seed {seed} (clipped: {clipped}) differs when merged"
            );
            assert!(merged_ops <= plain_ops, "merging never adds ops");
        }
    }
}

#[test]
fn wrapped_styled_text_matches_wrapped_plain_text() {
    let Some(mut scene) = Scene::new((128, 128)) else {
        return;
    };
    let layer = scene.layer();
    let params = FontParameters::new(12.0);
    let text = "MMM MMM MMM MMM";
    let spans = [
        TextSpan::new("MMM MMM ", font(), params, Color::rgb(255, 255, 255)),
        TextSpan::new("MMM MMM", font(), params, Color::rgb(255, 255, 255)),
    ];
    let (natural_width, natural_height) = layer.get_styled_text_size(&spans);
    if natural_width <= 0.0 {
        return;
    }
    let max = natural_width / 2.0;
    let wrapped = layer.get_wrapped_styled_text_size(&spans, max);
    assert!(
        wrapped.1 > natural_height * 1.5,
        "narrowing the width adds lines: {wrapped:?} vs {natural_height}"
    );
    assert!(wrapped.0 <= max + 0.5);
    // Splitting the string into spans does not change where it breaks.
    assert_eq!(
        wrapped,
        layer.get_wrapped_text_size(text, max, &font(), &params)
    );

    layer.draw_wrapped_styled_text(&spans, (0.0, 0.0), max, Alignment::default());
    scene.frame(1.0);
    let rows_with_ink = (0..wrapped.1 as u32)
        .filter(|&y| (0..=max as u32).any(|x| scene.pixel(x, y)[0] > 128))
        .count();
    assert!(
        rows_with_ink as f32 > wrapped.1 * 0.5,
        "every wrapped line is drawn"
    );
}

#[test]
fn a_wrapped_span_keeps_its_own_style_across_the_break() {
    let Some(mut scene) = Scene::new((128, 128)) else {
        return;
    };
    let layer = scene.layer();
    let params = FontParameters::new(12.0);
    // One red span long enough to wrap on its own.
    let spans = [TextSpan::new(
        "MMM MMM MMM MMM",
        font(),
        params,
        Color::rgb(255, 0, 0),
    )];
    let max = layer.get_styled_text_size(&spans).0 / 3.0;
    if max <= 0.0 {
        return;
    }
    layer.draw_wrapped_styled_text(&spans, (0.0, 0.0), max, Alignment::default());
    scene.frame(1.0);
    let height = layer.get_wrapped_styled_text_size(&spans, max).1 as u32;
    let mut red = 0;
    for y in 0..height {
        for x in 0..=max as u32 {
            let p = scene.pixel(x, y);
            if p[0] > 128 {
                assert!(p[1] < 64 && p[2] < 64, "ink stays red at {x},{y}: {p:?}");
                red += 1;
            }
        }
    }
    assert!(red > 0);
}

#[test]
fn wrapped_styled_text_with_a_wider_limit_is_unchanged() {
    let Some(scene) = Scene::new((128, 128)) else {
        return;
    };
    let mut scene = scene;
    let layer = scene.layer();
    let spans = [TextSpan::new(
        "short",
        font(),
        FontParameters::new(12.0),
        Color::rgb(255, 255, 255),
    )];
    assert_eq!(
        layer.get_wrapped_styled_text_size(&spans, 10_000.0),
        layer.get_styled_text_size(&spans)
    );
}

#[test]
fn long_merged_text_runs_are_cut_into_bounded_batches() {
    let Some(mut scene) = Scene::new((128, 128)) else {
        return;
    };
    let layer = scene.layer();
    for line in 0..40 {
        let y = (line % 10) as f32 * 12.0;
        layer.draw_rectangle(
            (line as f32 * 3.0, y),
            (2.0, 12.0),
            Color::rgb(40, 40, 90),
            Rounding::NONE,
        );
        label(&layer, ".", (line as f32 * 3.0, y));
    }
    scene.frame(1.0);
    let kinds = layer.0.borrow().op_kinds_for_test();
    // 40 text commands at 16 per batch: 16 + 16 + 8.
    assert_eq!(kinds, ["solid", "text", "text", "text"]);
}
