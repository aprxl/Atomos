//! Clip-scope pixel tests: scissor and stencil paths, nesting, DPI and
//! antialiasing. Each skips when no GPU adapter is available.

use super::test_support::Scene;
use super::{Alignment, ClipShape, Color, Font, FontParameters, Layer, Rounding};

fn scene() -> Option<Scene> {
    Scene::new((64, 64))
}

fn assert_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
    for channel in 0..4 {
        assert!(
            actual[channel].abs_diff(expected[channel]) <= tolerance,
            "pixel {actual:?} is not within {tolerance} of {expected:?}"
        );
    }
}

fn white(layer: &Layer) {
    layer.draw_rectangle(
        (0.0, 0.0),
        (64.0, 64.0),
        Color::rgb(255, 255, 255),
        Rounding::NONE,
    );
}

fn rect(at: (f32, f32), size: (f32, f32)) -> ClipShape {
    ClipShape::Rectangle {
        top_left: at,
        size,
        rounding: Rounding::NONE,
    }
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn a_rect_scope_clips_only_what_it_draws() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((10.0, 10.0), (20.0, 20.0)), white)
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(15, 15), WHITE, 1);
    assert_near(scene.pixel(5, 5), BLACK, 0);
    assert_near(scene.pixel(40, 40), BLACK, 0);
}

#[test]
fn content_after_a_scope_is_unclipped() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((0.0, 0.0), (10.0, 10.0)), |l| {
            l.draw_rectangle(
                (0.0, 0.0),
                (64.0, 64.0),
                Color::rgb(255, 0, 0),
                Rounding::NONE,
            )
        })
        .unwrap();
    layer.draw_rectangle(
        (50.0, 50.0),
        (10.0, 10.0),
        Color::rgb(0, 0, 255),
        Rounding::NONE,
    );
    scene.frame(1.0);
    assert_near(scene.pixel(55, 55), [0, 0, 255, 255], 1);
    assert_near(scene.pixel(30, 30), BLACK, 0);
}

#[test]
fn nested_rect_scopes_intersect() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((0.0, 0.0), (30.0, 30.0)), |l| {
            l.with_clip(rect((20.0, 20.0), (30.0, 30.0)), white)
                .unwrap()
        })
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(25, 25), WHITE, 1);
    assert_near(scene.pixel(10, 10), BLACK, 0);
    assert_near(scene.pixel(40, 40), BLACK, 0);
}

#[test]
fn an_empty_intersection_draws_nothing_and_recovers() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((0.0, 0.0), (10.0, 10.0)), |l| {
            l.with_clip(rect((40.0, 40.0), (10.0, 10.0)), white)
                .unwrap()
        })
        .unwrap();
    layer.draw_rectangle(
        (50.0, 0.0),
        (10.0, 10.0),
        Color::rgb(0, 255, 0),
        Rounding::NONE,
    );
    scene.frame(1.0);
    assert_near(scene.pixel(5, 5), BLACK, 0);
    assert_near(scene.pixel(45, 45), BLACK, 0);
    assert_near(scene.pixel(55, 5), [0, 255, 0, 255], 1);
}

#[test]
fn a_scope_follows_a_dpi_change() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((0.0, 0.0), (8.0, 8.0)), white)
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(12, 4), BLACK, 0);
    scene.frame(2.0);
    assert_near(scene.pixel(12, 4), WHITE, 1);
    assert_near(scene.pixel(20, 4), BLACK, 0);
}

#[test]
fn a_circle_scope_clips_by_stencil() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(
            ClipShape::Circle {
                center: (32.0, 32.0),
                radius: 10.0,
            },
            white,
        )
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(32, 32), WHITE, 1);
    assert_near(scene.pixel(32, 50), BLACK, 0);
    assert_near(scene.pixel(5, 5), BLACK, 0);
}

#[test]
fn nested_circles_intersect_and_pop_restores_the_parent() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    let big = ClipShape::Circle {
        center: (32.0, 32.0),
        radius: 28.0,
    };
    let small = ClipShape::Circle {
        center: (32.0, 32.0),
        radius: 6.0,
    };
    layer
        .with_clip(big, |l| {
            l.with_clip(small, |l| {
                l.draw_rectangle(
                    (0.0, 0.0),
                    (64.0, 64.0),
                    Color::rgb(255, 0, 0),
                    Rounding::NONE,
                )
            })
            .unwrap();
            // Back in the big circle only.
            l.draw_rectangle(
                (0.0, 30.0),
                (64.0, 4.0),
                Color::rgb(0, 0, 255),
                Rounding::NONE,
            );
        })
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(32, 20), BLACK, 0);
    assert_near(scene.pixel(10, 31), [0, 0, 255, 255], 1);
    assert_near(scene.pixel(2, 31), BLACK, 0);
    assert_near(scene.pixel(32, 28), [255, 0, 0, 255], 1);
}

#[test]
fn a_shape_clip_edge_is_antialiased() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(
            ClipShape::Circle {
                center: (32.0, 32.0),
                radius: 15.5,
            },
            white,
        )
        .unwrap();
    scene.frame(1.0);
    // The circle's edge crosses the middle of pixel x = 47.
    let edge = scene.pixel(47, 32)[0];
    assert!(edge > 20 && edge < 235, "edge pixel was {edge}");
}

#[test]
fn a_fractional_rect_edge_is_antialiased_too() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(rect((10.5, 10.0), (20.0, 20.0)), white)
        .unwrap();
    scene.frame(1.0);
    let edge = scene.pixel(10, 20)[0];
    assert!(edge > 20 && edge < 235, "edge pixel was {edge}");
    assert_near(scene.pixel(20, 20), WHITE, 1);
}

#[test]
fn a_rounded_rect_scope_cuts_the_corner() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer
        .with_clip(
            ClipShape::Rectangle {
                top_left: (0.0, 0.0),
                size: (40.0, 40.0),
                rounding: Rounding::uniform(16.0),
            },
            white,
        )
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(0, 0), BLACK, 0);
    assert_near(scene.pixel(20, 20), WHITE, 1);
}

fn any_ink(scene: &Scene) -> bool {
    (0..64u32)
        .flat_map(|y| (0..64u32).map(move |x| (x, y)))
        .any(|(x, y)| scene.pixel(x, y)[0] > 50)
}

#[test]
fn text_is_clipped_by_a_shape() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    let draw = |l: &Layer| {
        l.draw_text(
            "MMMM",
            (0.0, 0.0),
            Color::rgb(255, 255, 255),
            Alignment::default(),
            Font::Named("sans-serif".into()),
            FontParameters::new(30.0),
        )
    };
    draw(&layer);
    scene.frame(1.0);
    if !any_ink(&scene) {
        return;
    }
    layer.clear();
    layer
        .with_clip(
            ClipShape::Circle {
                center: (500.0, 500.0),
                radius: 5.0,
            },
            draw,
        )
        .unwrap();
    scene.frame(1.0);
    assert!(!any_ink(&scene), "no text may survive an excluding clip");
}

#[test]
fn a_panicking_scope_still_closes() {
    let Some(mut scene) = scene() else { return };
    let first = scene.layer();
    let second = scene.layer();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        first
            .with_clip(rect((0.0, 0.0), (4.0, 4.0)), |_| panic!("boom"))
            .unwrap();
    }));
    white(&first);
    white(&second);
    scene.frame(1.0);
    assert_near(scene.pixel(30, 30), WHITE, 1);
}

#[test]
fn clip_scopes_change_the_automatic_invalidation_hash() {
    use super::draw_command::DrawCommand;
    use std::hash::{DefaultHasher, Hash, Hasher};
    let hash = |c: &DrawCommand| {
        let mut h = DefaultHasher::new();
        c.hash(&mut h);
        h.finish()
    };
    let a = DrawCommand::PushClip(ClipShape::Circle {
        center: (1.0, 1.0),
        radius: 2.0,
    });
    let b = DrawCommand::PushClip(ClipShape::Circle {
        center: (1.0, 1.0),
        radius: 3.0,
    });
    assert_ne!(hash(&a), hash(&b));
    assert_ne!(hash(&a), hash(&DrawCommand::PopClip));
}
