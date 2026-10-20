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
        .set_clip(Some(ClipShape::Circle {
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

#[test]
fn the_glyph_id_space_is_recycled_before_it_runs_out() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    scene.text().set_glyph_ids_in_use_for_test(62_000);
    layer.draw_text(
        "hi",
        (0.0, 0.0),
        Color::rgb(255, 255, 255),
        super::Alignment::default(),
        super::Font::Named("sans-serif".into()),
        super::FontParameters::new(16.0),
    );
    scene.frame(1.0);
    assert!(
        scene.text().glyph_ids_in_use_for_test() < 1_000,
        "IDs should restart from zero after the reset"
    );
}

fn passes(layer: &super::Layer) -> (u32, u32) {
    let inner = layer.0.borrow();
    (inner.content_passes, inner.effect_passes)
}

#[test]
fn a_clean_layer_is_composited_without_re_rendering() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::IDENTITY)));
    scene.frame(1.0);
    assert_eq!(passes(&layer), (1, 1));

    scene.frame(1.0);
    scene.frame(1.0);
    assert_eq!(passes(&layer), (1, 1), "clean frames run no layer passes");
    assert_near(scene.pixel(4, 4), [255, 0, 0, 255], 1);

    // The layer clip applies when the group is composited, so changing it
    // costs no content or effect pass.
    layer
        .set_clip(Some(ClipShape::Rectangle {
            top_left: (0.0, 0.0),
            size: (4.0, 4.0),
            rounding: Rounding::NONE,
        }))
        .unwrap();
    scene.frame(1.0);
    assert_eq!(passes(&layer), (1, 1));
    assert_near(scene.pixel(6, 6), [0, 0, 0, 255], 0);
    assert_near(scene.pixel(2, 2), [255, 0, 0, 255], 1);
}

#[test]
fn changing_only_the_effect_reruns_only_the_effect() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::IDENTITY)));
    scene.frame(1.0);
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::invert())));
    scene.frame(1.0);
    assert_eq!(passes(&layer), (1, 2));
    assert_near(scene.pixel(4, 4), [0, 255, 255, 255], 1);
}

#[test]
fn a_dpi_change_rerenders_a_retained_layer() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.set_cache_policy(super::CachePolicy::Always);
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    scene.frame(1.0);
    scene.frame(2.0);
    assert_eq!(passes(&layer).0, 2);
    assert_near(scene.pixel(12, 4), [255, 0, 0, 255], 1);
}

#[test]
fn animating_effect_parameters_reuses_the_effect_textures() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::IDENTITY)));
    scene.frame(1.0);
    let before = layer.0.borrow().effect_rebuilds;
    layer.set_effect(Some(ShaderEffect::ColorMatrix(ColorMatrix::invert())));
    scene.frame(1.0);
    assert_eq!(layer.0.borrow().effect_rebuilds, before);
    assert_near(scene.pixel(4, 4), [0, 255, 255, 255], 1);
}

mod text_layout {
    use super::*;
    use crate::renderer::{Font, FontParameters, TextLayout};

    /// Lay out `text` with a system font, or `None` on a machine with no
    /// adapter or no usable font (the tests then pass vacuously).
    fn laid_out(text: &str) -> Option<TextLayout> {
        let mut scene = scene()?;
        let layer = scene.layer();
        let layout = layer.layout_text(
            text,
            &Font::Named("sans-serif".into()),
            &FontParameters::new(20.0),
        );
        (layout.size().0 > 0.0).then_some(layout)
    }

    #[test]
    fn carets_advance_across_a_line_and_end_at_its_width() {
        let Some(layout) = laid_out("hello") else {
            return;
        };
        let xs: Vec<f32> = (0..=5).map(|b| layout.caret_rect(b).x).collect();
        assert_eq!(xs[0], 0.0);
        assert!(xs.windows(2).all(|w| w[1] > w[0]), "{xs:?}");
        assert!((xs[5] - layout.size().0).abs() < 0.5, "{xs:?}");
    }

    #[test]
    fn hit_testing_inverts_caret_positions() {
        let Some(layout) = laid_out("hello") else {
            return;
        };
        for byte in 0..=5 {
            let rect = layout.caret_rect(byte);
            let y = rect.y + rect.height / 2.0;
            assert_eq!(layout.hit_test((rect.x, y)), byte);
        }
        assert_eq!(layout.hit_test((-50.0, 5.0)), 0);
        assert_eq!(layout.hit_test((5000.0, 5.0)), 5);
    }

    #[test]
    fn a_selection_spans_its_carets() {
        let Some(layout) = laid_out("hello") else {
            return;
        };
        let rects = layout.selection_rects(1..4);
        assert_eq!(rects.len(), 1);
        let (a, b) = (layout.caret_rect(1).x, layout.caret_rect(4).x);
        assert!((rects[0].x - a).abs() < 0.5);
        assert!((rects[0].x + rects[0].width - b).abs() < 0.5);
        assert!(layout.selection_rects(2..2).is_empty());
    }

    #[test]
    fn lines_split_at_newlines_including_empty_ones() {
        let Some(layout) = laid_out("ab\n\ncd") else {
            return;
        };
        assert_eq!(layout.lines().len(), 3);
        assert_eq!(layout.lines()[2].range, 4..6);
        let (first, third) = (layout.caret_rect(0), layout.caret_rect(4));
        assert!(third.y > first.y);
        assert_eq!(layout.hit_test((5000.0, 10_000.0)), 6);
        let across = layout.selection_rects(1..5);
        assert_eq!(across.len(), 2, "the empty line paints nothing: {across:?}");
    }

    #[test]
    fn a_combining_mark_is_not_a_caret_stop() {
        // "e" + combining acute shapes as one cluster of 3 bytes.
        let Some(layout) = laid_out("e\u{301}x") else {
            return;
        };
        assert_eq!(layout.caret_rect(1).x, layout.caret_rect(0).x);
        assert!(layout.caret_rect(3).x > layout.caret_rect(0).x);
    }

    #[test]
    fn right_to_left_text_runs_leftward() {
        let Some(layout) = laid_out("\u{5d0}\u{5d1}\u{5d2}") else {
            return;
        };
        let (start, end) = (layout.caret_rect(0).x, layout.caret_rect(6).x);
        assert!(start > end, "logical start is on the right: {start} {end}");
    }
}

mod painter_order {
    use super::*;
    use crate::renderer::{Alignment, Font, FontParameters, Pixels};

    fn text(layer: &super::super::Layer, at: (f32, f32), size: f32) {
        layer.draw_text(
            "MMMM",
            at,
            Color::rgb(255, 255, 255),
            Alignment::default(),
            Font::Named("sans-serif".into()),
            FontParameters::new(size),
        );
    }

    #[test]
    fn an_image_then_a_covering_rect_shows_the_rect() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        layer.draw_image(
            Pixels::new(1, 1, vec![255; 4]),
            (0.0, 0.0),
            (20.0, 20.0),
            Color::rgb(255, 255, 255),
        );
        layer.draw_rectangle(
            (0.0, 0.0),
            (20.0, 20.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        scene.frame(1.0);
        assert_near(scene.pixel(4, 4), [0, 0, 255, 255], 1);
    }

    #[test]
    fn a_rect_then_an_image_shows_the_image() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        layer.draw_rectangle(
            (0.0, 0.0),
            (20.0, 20.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        layer.draw_image(
            Pixels::new(1, 1, vec![255; 4]),
            (0.0, 0.0),
            (20.0, 20.0),
            Color::rgb(255, 255, 255),
        );
        scene.frame(1.0);
        assert_near(scene.pixel(4, 4), [255, 255, 255, 255], 1);
    }

    /// First pixel inside the glyph run that is clearly inked, if any.
    fn inked(scene: &Scene) -> Option<(u32, u32)> {
        (0..40u32)
            .flat_map(|y| (0..64u32).map(move |x| (x, y)))
            .find(|&(x, y)| scene.pixel(x, y)[0] > 200)
    }

    #[test]
    fn text_then_a_covering_rect_hides_the_text() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        text(&layer, (0.0, 0.0), 30.0);
        scene.frame(1.0);
        let Some((x, y)) = inked(&scene) else { return };

        layer.clear();
        text(&layer, (0.0, 0.0), 30.0);
        layer.draw_rectangle(
            (0.0, 0.0),
            (64.0, 64.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        scene.frame(1.0);
        assert_near(scene.pixel(x, y), [0, 0, 255, 255], 1);
    }

    #[test]
    fn a_rect_text_rect_sandwich_keeps_the_text_between() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        text(&layer, (0.0, 0.0), 30.0);
        scene.frame(1.0);
        let Some((x, y)) = inked(&scene) else { return };

        layer.clear();
        layer.draw_rectangle(
            (0.0, 0.0),
            (64.0, 64.0),
            Color::rgb(255, 0, 0),
            Rounding::NONE,
        );
        text(&layer, (0.0, 0.0), 30.0);
        layer.draw_rectangle(
            (60.0, 60.0),
            (4.0, 4.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        scene.frame(1.0);
        assert_near(scene.pixel(x, y), [255, 255, 255, 255], 60);
        assert_near(scene.pixel(62, 62), [0, 0, 255, 255], 1);
        assert_near(scene.pixel(1, 63), [255, 0, 0, 255], 1);
    }

    #[test]
    fn two_text_layers_in_one_frame_both_draw() {
        let Some(mut scene) = scene() else { return };
        let first = scene.layer();
        text(&first, (0.0, 0.0), 30.0);
        scene.frame(1.0);
        let Some((x, y)) = inked(&scene) else { return };

        let second = scene.layer();
        text(&second, (0.0, 30.0), 30.0);
        first.clear();
        text(&first, (0.0, 0.0), 30.0);
        scene.frame(1.0);
        assert!(scene.pixel(x, y)[0] > 200, "first layer text survived");
        assert!(
            (30..64u32)
                .flat_map(|y| (0..64u32).map(move |x| (x, y)))
                .any(|(x, y)| scene.pixel(x, y)[0] > 200),
            "second layer text drew"
        );
    }

    #[test]
    fn an_atlas_reset_mid_prepare_keeps_earlier_text() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        // Two text runs separated by a shape: two batches in one layer.
        text(&layer, (0.0, 0.0), 30.0);
        layer.draw_rectangle(
            (60.0, 60.0),
            (2.0, 2.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        text(&layer, (0.0, 32.0), 30.0);
        scene.frame(1.0);
        let Some((x, y)) = inked(&scene) else { return };

        // Force the next prepare to reset the shared atlas partway through.
        layer.clear();
        scene.text().set_glyph_ids_in_use_for_test(62_000);
        text(&layer, (0.0, 0.0), 30.0);
        layer.draw_rectangle(
            (60.0, 60.0),
            (2.0, 2.0),
            Color::rgb(0, 0, 255),
            Rounding::NONE,
        );
        text(&layer, (0.0, 32.0), 31.0);
        scene.frame(1.0);
        assert!(scene.pixel(x, y)[0] > 100, "first batch survived a reset");
    }

    #[test]
    fn an_unchanged_text_batch_is_not_prepared_again() {
        let Some(mut scene) = scene() else { return };
        let layer = scene.layer();
        text(&layer, (0.0, 0.0), 30.0);
        scene.frame(1.0);
        let prepared = layer.0.borrow().text_batch_states_for_test();
        assert_eq!(prepared.len(), 1);
        scene.frame(1.0);
        assert_eq!(layer.0.borrow().text_batch_states_for_test(), prepared);
        assert_eq!(layer.0.borrow().geometry_rebuilds, 1);
    }
}

#[test]
fn a_clean_direct_layer_does_no_cpu_work_and_owns_no_target() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    scene.frame(1.0);
    scene.frame(1.0);
    scene.frame(1.0);
    let inner = layer.0.borrow();
    assert_eq!(inner.geometry_rebuilds, 1);
    assert_eq!(
        inner.content_passes, 0,
        "direct layers have no content pass"
    );
    assert_eq!(inner.texture_size_for_test(), (1, 1));
    drop(inner);
    assert_near(scene.pixel(4, 4), [255, 0, 0, 255], 1);
}

#[test]
fn a_direct_layer_follows_a_dpi_change_with_one_rebuild() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (8.0, 8.0),
        Color::rgb(255, 0, 0),
        Rounding::NONE,
    );
    scene.frame(1.0);
    scene.frame(2.0);
    scene.frame(2.0);
    assert_eq!(layer.0.borrow().geometry_rebuilds, 2);
    assert_near(scene.pixel(12, 4), [255, 0, 0, 255], 1);
}

#[test]
fn a_layer_with_an_effect_becomes_a_group_and_back() {
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
    assert_eq!(layer.0.borrow().texture_size_for_test(), (64, 64));
    assert_near(scene.pixel(4, 4), [0, 255, 255, 255], 1);

    layer.set_effect(None);
    scene.frame(1.0);
    assert_eq!(layer.0.borrow().texture_size_for_test(), (1, 1));
    assert_near(scene.pixel(4, 4), [255, 0, 0, 255], 1);
}

#[test]
fn groups_and_direct_layers_keep_their_z_order() {
    let Some(mut scene) = scene() else { return };
    let bottom = scene.layer();
    let middle = scene.layer();
    let top = scene.layer();
    for (layer, color) in [
        (&bottom, Color::rgb(255, 0, 0)),
        (&middle, Color::rgb(0, 255, 0)),
        (&top, Color::rgb(0, 0, 255)),
    ] {
        layer.draw_rectangle((0.0, 0.0), (20.0, 20.0), color, Rounding::NONE);
    }
    // The middle layer is a cached group; the others draw directly.
    middle.set_cache_policy(super::CachePolicy::Always);
    scene.frame(1.0);
    assert_near(scene.pixel(4, 4), [0, 0, 255, 255], 1);
    top.clear();
    scene.frame(1.0);
    assert_near(scene.pixel(4, 4), [0, 255, 0, 255], 1);
}

#[test]
fn a_blurred_layer_is_clipped_after_the_blur() {
    let Some(mut scene) = scene() else { return };
    let layer = scene.layer();
    layer.draw_rectangle(
        (0.0, 0.0),
        (64.0, 64.0),
        Color::rgb(255, 255, 255),
        Rounding::NONE,
    );
    layer.set_effect(Some(ShaderEffect::Blur { radius: 4.0 }));
    layer
        .set_clip(Some(ClipShape::Circle {
            center: (32.0, 32.0),
            radius: 10.0,
        }))
        .unwrap();
    scene.frame(1.0);
    assert_near(scene.pixel(32, 32), [255, 255, 255, 255], 2);
    assert_near(scene.pixel(32, 50), [0, 0, 0, 255], 0);
}
