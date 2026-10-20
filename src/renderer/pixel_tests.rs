//! Pixel-level regression tests against a headless GPU. Each skips (passes)
//! when no adapter is available; see [`super::test_support`].

use super::test_support::Scene;
use super::{ClipShape, Color, ColorMatrix, Rounding, ShaderEffect};

fn scene() -> Option<Scene> {
    Scene::new((64, 64))
}

/// Within `tolerance` per channel: GPU blending rounds differently across
/// vendors.
fn assert_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
    for channel in 0..4 {
        assert!(
            actual[channel].abs_diff(expected[channel]) <= tolerance,
            "pixel {actual:?} is not within {tolerance} of {expected:?}"
        );
    }
}

#[test]
fn authored_srgb_colors_reach_the_surface_unchanged() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (64.0, 64.0),
        Color::rgb(27, 27, 27),
        Rounding::NONE,
    );
    scene.frame(1.0);
    assert_near(scene.pixel(4, 4), [27, 27, 27, 255], 1);
}

#[test]
fn a_translucent_shape_tints_what_is_under_it() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (20.0, 20.0),
        Color::rgb(0, 0, 255),
        Rounding::NONE,
    );
    layer.draw_rectangle(
        (0.0, 0.0),
        (20.0, 20.0),
        Color::rgba(255, 0, 0, 128),
        Rounding::NONE,
    );
    scene.frame(1.0);
    // Linear-light mix of red and blue at 50%, re-encoded to sRGB: ~188.
    assert_near(scene.pixel(4, 4), [188, 0, 188, 255], 2);
}

#[test]
fn a_translucent_shape_over_nothing_composites_once() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (20.0, 20.0),
        Color::rgba(255, 255, 255, 128),
        Rounding::NONE,
    );
    scene.frame(1.0);
    // 50% white over black, mixed in linear light: ~188. Applying alpha
    // twice (a straight-alpha composite of a premultiplied layer) gives ~137.
    assert_near(scene.pixel(4, 4), [188, 188, 188, 255], 2);
}

#[test]
fn a_clip_shape_cuts_the_layer_outside_it() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (64.0, 64.0),
        Color::rgb(255, 255, 255),
        Rounding::NONE,
    );
    layer
        .set_clip_shape(Some(ClipShape::Circle {
            center: (32.0, 32.0),
            radius: 10.0,
        }))
        .expect("circle clip is valid");
    scene.frame(1.0);
    assert_near(scene.pixel(32, 32), [255, 255, 255, 255], 1);
    assert_near(scene.pixel(2, 2), [0, 0, 0, 255], 0);
}

#[test]
fn retained_content_follows_a_dpi_change() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    scene.frame(1.0);
    assert_near(scene.pixel(12, 4), [0, 0, 0, 255], 0);

    // No redraw: the layer holds logical commands, so doubling the scale
    // must double the rectangle to 16x16 physical pixels.
    scene.frame(2.0);
    assert_near(scene.pixel(12, 4), [255, 0, 0, 255], 1);
    assert_near(scene.pixel(20, 4), [0, 0, 0, 255], 0);
}

#[test]
fn content_drawn_before_the_first_render_uses_the_scale_it_renders_at() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    scene.frame(2.0);
    // Drawn while the layer still reported scale 1, rendered at 2.
    assert_near(scene.pixel(12, 4), [255, 0, 0, 255], 1);
}

#[test]
fn a_color_matrix_leaves_transparent_texels_transparent() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::invert())));
    scene.frame(1.0);
    // Inverting transparent black yields white at alpha zero, which must
    // not leak into the frame.
    assert_near(scene.pixel(32, 32), [0, 0, 0, 255], 0);
    // Inside the content, red inverts to cyan.
    assert_near(scene.pixel(4, 4), [0, 255, 255, 255], 1);
}

#[test]
fn an_identity_matrix_preserves_translucent_edges() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (20.0, 20.0),
        Color::rgba(255, 255, 255, 128),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::IDENTITY)));
    scene.frame(1.0);
    assert_near(scene.pixel(4, 4), [188, 188, 188, 255], 3);
}

#[test]
fn a_matrix_can_generate_alpha_from_nothing() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    // Constant opaque blue regardless of input.
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix([
        0.0, 0.0, 0.0, 0.0, 0.0, //
        0.0, 0.0, 0.0, 0.0, 0.0, //
        0.0, 0.0, 0.0, 0.0, 1.0, //
        0.0, 0.0, 0.0, 0.0, 1.0,
    ]))));
    scene.frame(1.0);
    assert_near(scene.pixel(32, 32), [0, 0, 255, 255], 0);
}
