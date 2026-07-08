//! [`TextStack`] — the glyphon/cosmic-text machinery shared by every
//! [`super::Layer`]'s `draw_text`.
//!
//! `FontSystem`/`SwashCache`/`TextAtlas`/`Viewport` are one shared instance,
//! not one per layer: `FontSystem` is expensive to duplicate (it walks the
//! system font database once at startup), and `TextAtlas` (the rasterized
//! glyph bitmap cache) is specifically designed to back multiple
//! `TextRenderer`s at once. `TextRenderer` itself is *not* shared, though —
//! see [`TextStack::create_text_renderer`]'s doc for why one shared
//! `TextRenderer` across layers is actually unsound (a real crash this
//! renderer hit): each layer gets its own via that method.
//!
//! **Every `draw_text` call is re-shaped and re-prepared every frame**,
//! regardless of whether its content changed — unlike every other
//! `DrawCommand`, text does not skip work on an unchanged `Automatic`
//! layer. This is a deliberate simplification, not an oversight: glyphon's
//! `prepare()`/`render()` pair must run back-to-back for a given layer
//! (prepare uploads this frame's glyphs into the shared atlas; render
//! draws whatever was just uploaded), and since every layer's render pass
//! shares that one atlas, skipping `prepare()` for an "unchanged" layer
//! risks drawing glyphs a *different* layer's `prepare()` already evicted.
//! Always re-preparing right before rendering, one layer fully at a time,
//! sidesteps that risk entirely. Fine for icon-sized labels; a real cost
//! for large blocks of frequently-redrawn text — revisiting this (e.g. a
//! dedicated atlas per layer) is future work.

use std::collections::hash_map::HashMap;
use std::path::{Path, PathBuf};

use glyphon::{
    fontdb, Attrs, Buffer, Cache, Color as GlyphonColor, Family, FontSystem, Metrics, Resolution,
    Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};

use super::color::Color;
use super::draw_command::DrawCommand;
use super::font::Font;
use super::font_parameters::FontParameters;
use super::glyph_effects::EffectGlyphs;
use super::text_span::TextSpan;
use super::{Alignment, HorizontalAlign, VerticalAlign, MSAA_SAMPLE_COUNT};

/// Line height as a multiple of font size — matches the ratio the
/// original Phase-0 text spike used.
const LINE_HEIGHT_RATIO: f32 = 1.25;

/// Cache key for [`TextStack::registered_fonts`]. `Bytes` is keyed by
/// pointer identity + length rather than content — same reasoning as
/// [`Font`]'s `Hash` impl.
#[derive(PartialEq, Eq, Hash)]
enum FontKey {
    Bytes(usize, usize),
    File(PathBuf),
}

impl FontKey {
    fn from_font(font: &Font) -> Option<Self> {
        match font {
            Font::Named(_) => None,
            Font::Bytes(bytes) => Some(FontKey::Bytes(bytes.as_ptr() as usize, bytes.len())),
            Font::File(path) => Some(FontKey::File(path.clone())),
        }
    }
}

pub(super) struct TextStack {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    effect_glyphs: EffectGlyphs,
    // Resolved family name for each not-yet-seen `Font::Bytes`/`Font::File`
    // — `None` means registration failed (logged once, at registration
    // time). `Font::Named` never needs an entry; it's already a name.
    registered_fonts: HashMap<FontKey, Option<String>>,
    // Every `TextArea` draws through `custom_glyphs` (so `FontParameters`'
    // weight/width apply uniformly, even at the neutral 0.0/1.0 defaults),
    // so every `TextArea` needs a `buffer` field despite never using its
    // glyphs directly. One permanently-empty `Buffer` shared by all of
    // them, rather than allocating a fresh empty one per `draw_text` call.
    blank_buffer: Buffer,
}

impl TextStack {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> Self {
        // `FontSystem::new` walks the system font database; on cold caches
        // it can take a second — paid once, here, not per layer.
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        // `Cache` holds the shared pipeline/bind group layouts a `TextAtlas`
        // and `Viewport` are built from — sharing it (rather than the atlas
        // itself, which is per-`TextRenderer`) is glyphon's own intended
        // pattern for multiple text-drawing surfaces.
        let glyph_cache = Cache::new(device);
        let atlas = TextAtlas::new(device, queue, &glyph_cache, format);
        let mut viewport = Viewport::new(device, &glyph_cache);
        viewport.update(queue, Resolution { width: size.0, height: size.1 });

        let blank_buffer = build_buffer(&mut font_system, "", &Attrs::new(), 16.0);

        Self {
            font_system,
            swash_cache,
            atlas,
            viewport,
            effect_glyphs: EffectGlyphs::new(),
            registered_fonts: HashMap::new(),
            blank_buffer,
        }
    }

    /// Build a new [`TextRenderer`] registered against this stack's shared
    /// [`TextAtlas`] — one per [`super::Layer`], not one shared instance.
    ///
    /// A `TextRenderer` owns exactly one vertex buffer, and
    /// `prepare_with_custom` calls `.destroy()` on it *immediately*
    /// (synchronously, not deferred to whenever the GPU actually gets
    /// around to it) when it needs to grow — see glyphon's
    /// `text_render.rs`. `render_layers` records every layer's render pass
    /// into one shared `wgpu::CommandEncoder` that isn't submitted until
    /// the whole frame is done, so if every layer shared one
    /// `TextRenderer`, an earlier layer's already-recorded
    /// `set_vertex_buffer` could end up pointing at a buffer a *later*
    /// layer's `prepare_with_custom` destroyed before the frame ever
    /// submits — a real crash this renderer hit as soon as more than one
    /// layer drew text in the same frame. Giving each layer its own
    /// `TextRenderer` (own vertex buffer) sidesteps this entirely, while
    /// the actually-expensive-to-duplicate state (`FontSystem`'s font
    /// database, `TextAtlas`'s rasterized glyph bitmaps) stays shared —
    /// `TextAtlas` is specifically designed to back multiple
    /// `TextRenderer`s at once (`TextRenderer::new` takes `&mut TextAtlas`
    /// for exactly this reason).
    pub(super) fn create_text_renderer(&mut self, device: &wgpu::Device) -> TextRenderer {
        TextRenderer::new(
            &mut self.atlas,
            device,
            wgpu::MultisampleState {
                count: MSAA_SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            None,
        )
    }

    pub(super) fn resize(&mut self, queue: &wgpu::Queue, size: (u32, u32)) {
        self.viewport.update(queue, Resolution { width: size.0, height: size.1 });
    }

    /// Measure the pixel size `text` would occupy shaped with `font`/
    /// `font_parameters`, before any [`Alignment`] offset is applied — the
    /// same measurement [`TextStack::prepare_layer`] uses internally to
    /// resolve alignment. Exposed via [`super::Layer::get_text_size`].
    pub(super) fn measure(&mut self, text: &str, font: &Font, font_parameters: &FontParameters) -> (f32, f32) {
        let buffer = self.shape(text, font, font_parameters);
        measure_shaped_buffer(&buffer)
    }

    /// Resolve a [`Font`] to the family name cosmic-text's `Attrs` should
    /// select, registering `Bytes`/`File` fonts with `FontSystem`'s
    /// `fontdb` the first time they're seen.
    fn resolve_family(&mut self, font: &Font) -> Option<String> {
        let Font::Named(name) = font else {
            let key = FontKey::from_font(font).expect("only Named has no FontKey");
            if let Some(cached) = self.registered_fonts.get(&key) {
                return cached.clone();
            }
            let resolved = match font {
                Font::Bytes(bytes) => register_font_bytes(self.font_system.db_mut(), bytes),
                Font::File(path) => match register_font_file(self.font_system.db_mut(), path) {
                    Ok(name) => name,
                    Err(e) => {
                        eprintln!("Atomos: failed to load font {}: {e}", path.display());
                        None
                    }
                },
                Font::Named(_) => unreachable!(),
            };
            self.registered_fonts.insert(key, resolved.clone());
            return resolved;
        };
        Some(name.clone())
    }

    fn shape(&mut self, text: &str, font: &Font, font_parameters: &FontParameters) -> Buffer {
        let family = self.resolve_family(font);
        let mut attrs = Attrs::new();
        if let Some(family) = &family {
            attrs = attrs.family(Family::Name(family));
        }
        attrs = attrs.letter_spacing(font_parameters.tracking);
        build_buffer(&mut self.font_system, text, &attrs, font_parameters.size)
    }

    /// Measure the combined bounding box `spans` would occupy shaped
    /// together, before any [`Alignment`] offset — the spans counterpart to
    /// [`TextStack::measure`]. Exposed via
    /// [`super::Layer::get_styled_text_size`].
    pub(super) fn measure_spans(&mut self, spans: &[TextSpan]) -> (f32, f32) {
        let (buffer, _) = self.shape_spans(spans);
        measure_shaped_buffer(&buffer)
    }

    /// Like [`TextStack::shape`], but for [`TextSpan`]s: every span is
    /// shaped together as a single cosmic-text rich-text buffer (one shape
    /// operation for the whole run, not one per span — the entire point of
    /// `draw_styled_text`), each keeping its own font/size/tracking/color.
    /// `weight`/`width` (this renderer's synthetic faux-bold/condense) have
    /// no native per-span cosmic-text attribute, so they're returned
    /// alongside the buffer as `span_effects`, indexed by the
    /// `Attrs::metadata` each span is tagged with below —
    /// `EffectGlyphs::layout_to_custom_glyphs_spans` recovers a glyph's
    /// effect via `span_effects[glyph.metadata]`. Color, size, and tracking
    /// all have native per-span `Attrs` fields, so they need no such
    /// workaround.
    fn shape_spans(&mut self, spans: &[TextSpan]) -> (Buffer, Vec<(f32, f32)>) {
        // Resolved before building any `Attrs` below, since those borrow
        // the resolved family names by reference — `resolve_family` needs
        // `&mut self` and can't be called again once that borrow starts.
        let families: Vec<Option<String>> =
            spans.iter().map(|span| self.resolve_family(&span.font)).collect();

        let span_effects: Vec<(f32, f32)> = spans
            .iter()
            .map(|span| (span.font_parameters.weight, span.font_parameters.width))
            .collect();

        let owned_attrs: Vec<(String, Attrs)> = spans
            .iter()
            .zip(&families)
            .enumerate()
            .map(|(index, (span, family))| {
                let mut attrs = Attrs::new();
                if let Some(family) = family {
                    attrs = attrs.family(Family::Name(family));
                }
                attrs = attrs.letter_spacing(span.font_parameters.tracking);
                attrs = attrs.metrics(Metrics::new(
                    span.font_parameters.size,
                    span.font_parameters.size * LINE_HEIGHT_RATIO,
                ));
                let Color::Solid(rgba) = span.color else {
                    unreachable!(
                        "draw_styled_text only accepts Color::Solid, enforced in Layer::draw_styled_text"
                    );
                };
                attrs = attrs.color(GlyphonColor::rgba(rgba[0], rgba[1], rgba[2], rgba[3]));
                attrs = attrs.metadata(index);
                (span.text.clone(), attrs)
            })
            .collect();

        // Only used as the buffer's initial/default metrics — every span
        // above carries its own `Attrs::metrics` override, so this never
        // actually applies to rendered glyphs. Just needs to be non-zero.
        let default_size = spans.first().map_or(16.0, |span| span.font_parameters.size);
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(default_size, default_size * LINE_HEIGHT_RATIO),
        );
        {
            let mut borrowed = buffer.borrow_with(&mut self.font_system);
            borrowed.set_size(None, None);
            borrowed.set_rich_text(
                owned_attrs.iter().map(|(text, attrs)| (text.as_str(), attrs.clone())),
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            borrowed.shape_until_scroll(true);
        }
        (buffer, span_effects)
    }

    /// Shape and rasterize every `DrawCommand::Text`/`StyledText` in
    /// `commands`, then hand the resulting glyphs to `text_renderer` (the
    /// caller `Layer`'s own — see [`TextStack::create_text_renderer`]) —
    /// called once per layer, immediately before that layer's own render
    /// pass (see the module doc for why the ordering matters). Layers with
    /// no text commands still call this (with an empty batch), so stale
    /// glyphs from a previous frame don't linger.
    pub(super) fn prepare_layer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        commands: &[DrawCommand],
        text_renderer: &mut TextRenderer,
    ) -> Result<(), glyphon::PrepareError> {
        let mut all_glyphs = Vec::new();
        for command in commands {
            match command {
                DrawCommand::Text {
                    text,
                    position,
                    font,
                    font_parameters,
                    alignment,
                    color,
                } => {
                    let buffer = self.shape(text, font, font_parameters);
                    let (width, height) = measure_shaped_buffer(&buffer);
                    let anchored = apply_alignment(*position, *alignment, width, height);
                    let rgba = match color {
                        Color::Solid(rgba) => *rgba,
                        _ => {
                            debug_assert!(
                                false,
                                "draw_text only supports Color::Solid — falling back to opaque white"
                            );
                            [0xFF, 0xFF, 0xFF, 0xFF]
                        }
                    };
                    let glyphs = self.effect_glyphs.layout_to_custom_glyphs(
                        &mut self.font_system,
                        &buffer,
                        (anchored[0], anchored[1]),
                        (font_parameters.weight, font_parameters.width),
                        rgba,
                    );
                    all_glyphs.push(glyphs);
                }
                DrawCommand::StyledText { spans, position, alignment } => {
                    let (buffer, span_effects) = self.shape_spans(spans);
                    let (width, height) = measure_shaped_buffer(&buffer);
                    let anchored = apply_alignment(*position, *alignment, width, height);
                    let glyphs = self.effect_glyphs.layout_to_custom_glyphs_spans(
                        &mut self.font_system,
                        &buffer,
                        (anchored[0], anchored[1]),
                        &span_effects,
                    );
                    all_glyphs.push(glyphs);
                }
                _ => continue,
            }
        }

        let text_areas = all_glyphs.iter().map(|glyphs| TextArea {
            buffer: &self.blank_buffer,
            left: 0.0,
            top: 0.0,
            scale: 1.0,
            bounds: TextBounds::default(),
            default_color: GlyphonColor::rgb(0xFF, 0xFF, 0xFF),
            custom_glyphs: glyphs,
        });

        // `effect_glyphs` is bound as a local so the closure below captures
        // just this reference, not `self` — `self.font_system`/`self.atlas`
        // need their own simultaneous `&mut self` borrows below.
        let effect_glyphs = &self.effect_glyphs;
        text_renderer.prepare_with_custom(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
            |request| effect_glyphs.rasterize(request),
        )
    }

    /// Draw whatever [`TextStack::prepare_layer`] just uploaded into
    /// `text_renderer` (the caller `Layer`'s own) into the currently active
    /// render pass.
    pub(super) fn render(
        &self,
        text_renderer: &TextRenderer,
        rpass: &mut wgpu::RenderPass<'_>,
    ) -> Result<(), glyphon::RenderError> {
        text_renderer.render(&self.atlas, &self.viewport, rpass)
    }
}

fn build_buffer(font_system: &mut FontSystem, text: &str, attrs: &Attrs, size_px: f32) -> Buffer {
    let metrics = Metrics::new(size_px, size_px * LINE_HEIGHT_RATIO);
    let mut buffer = Buffer::new(font_system, metrics);
    {
        let mut borrowed = buffer.borrow_with(font_system);
        borrowed.set_size(None, None);
        borrowed.set_text(text, attrs, Shaping::Advanced, None);
        borrowed.shape_until_scroll(true);
    }
    buffer
}

/// Total width (widest line) and height (bottom edge of the last line) of
/// an already-shaped buffer.
fn measure_shaped_buffer(buffer: &Buffer) -> (f32, f32) {
    let mut width: f32 = 0.0;
    let mut height: f32 = 0.0;
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        height = height.max(run.line_top + run.line_height);
    }
    (width, height)
}

/// Shift an anchor position so it represents `alignment`'s point of a
/// `width` x `height` box instead of always its top-left.
fn apply_alignment(position: [f32; 2], alignment: Alignment, width: f32, height: f32) -> [f32; 2] {
    let dx = match alignment.horizontal {
        HorizontalAlign::Left => 0.0,
        HorizontalAlign::Center => -width / 2.0,
        HorizontalAlign::Right => -width,
    };
    let dy = match alignment.vertical {
        VerticalAlign::Top => 0.0,
        VerticalAlign::Center => -height / 2.0,
        VerticalAlign::Bottom => -height,
    };
    [position[0] + dx, position[1] + dy]
}

/// Register font bytes with `db` if not already present, returning the
/// resolved family name to select it by. Discovers the name by diffing
/// `db`'s faces before/after — `load_font_data` doesn't return the ID(s) it
/// added directly.
fn register_font_bytes(db: &mut fontdb::Database, bytes: &'static [u8]) -> Option<String> {
    let before: std::collections::HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_data(bytes.to_vec());
    db.faces()
        .find(|f| !before.contains(&f.id))
        .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
}

/// [`register_font_bytes`], loading from a file instead.
fn register_font_file(db: &mut fontdb::Database, path: &Path) -> Result<Option<String>, std::io::Error> {
    let before: std::collections::HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_file(path)?;
    Ok(db
        .faces()
        .find(|f| !before.contains(&f.id))
        .and_then(|f| f.families.first().map(|(name, _)| name.clone())))
}
