//! [`TextLayout`]: where the shaped glyphs of a `draw_text` string actually
//! land, for editing and interaction.
//!
//! Drawing text and *interacting* with it need the same geometry: where the
//! caret sits at a byte offset, which offset a click lands on, what
//! rectangles a selection covers. Measuring prefixes to answer those breaks
//! on ligatures, combining marks and right-to-left runs, so this reads the
//! answer from the shaped layout (cosmic-text's runs) instead, including the
//! same per-glyph advance corrections the synthetic weight/width effects
//! apply when drawing, so a caret never drifts from the ink.
//!
//! Everything is in **logical pixels, relative to the text's top-left** (the
//! position a `Alignment::default()` `draw_text` would be given). Offsets
//! into the text are UTF-8 byte offsets on character boundaries. Text breaks
//! lines only at `\n`; wrapping to a width is not part of this layout.

use std::ops::Range;

use super::glyph_effects::{GlyphEffect, effect_advance_delta};
use glyphon::Buffer;
use unicode_segmentation::UnicodeSegmentation;

/// An axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One caret stop within a line: the x position of the boundary before the
/// grapheme starting at `byte`.
#[derive(Clone, Copy, Debug)]
struct CaretStop {
    byte: usize,
    x: f32,
}

/// One drawn line of the text, split at `\n`.
#[derive(Clone, Debug)]
pub struct TextLine {
    /// Byte range of the line within the text, excluding its `\n`.
    pub range: Range<usize>,
    /// Top of the line box.
    pub top: f32,
    pub height: f32,
    /// Width of the line including synthetic-effect advance corrections.
    pub width: f32,
    /// Caret stops in logical (reading) order, one per grapheme boundary
    /// including the end of the line. Visual x is not monotonic in
    /// right-to-left runs.
    stops: Vec<CaretStop>,
    /// Glyph boxes `(byte range, x, width)` in visual order, for selection.
    cells: Vec<(Range<usize>, f32, f32)>,
}

/// The shaped geometry of one `draw_text` string. See the module docs.
#[derive(Clone, Debug)]
pub struct TextLayout {
    lines: Vec<TextLine>,
    size: (f32, f32),
}

impl TextLayout {
    /// Build from a shaped buffer, in the buffer's own (physical) pixels.
    pub(in crate::renderer) fn from_buffer(
        buffer: &Buffer,
        text: &str,
        effect: GlyphEffect,
    ) -> Self {
        // Byte offset at which each `\n`-separated paragraph begins.
        let mut line_starts = vec![0usize];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));

        // Visual lines per paragraph: one without wrapping, several when a
        // long paragraph breaks.
        let mut paragraphs: Vec<Vec<TextLine>> = vec![Vec::new(); line_starts.len()];
        let mut width: f32 = 0.0;
        let mut height: f32 = 0.0;

        for run in buffer.layout_runs() {
            let Some(&start) = line_starts.get(run.line_i) else {
                continue;
            };
            let visual = &mut paragraphs[run.line_i];
            let mut cells = Vec::new();
            let mut pen_delta = 0.0f32;
            // Byte span the glyphs cover, relative to the paragraph.
            let (mut first, mut last) = (usize::MAX, 0usize);
            for glyph in run.glyphs {
                let delta =
                    effect_advance_delta(effect.embolden.max(0.0), effect.condense, glyph.w);
                cells.push((
                    start + glyph.start..start + glyph.end,
                    glyph.x + pen_delta,
                    glyph.w + delta,
                ));
                pen_delta += delta;
                first = first.min(glyph.start);
                last = last.max(glyph.end);
            }
            // A paragraph's first line starts at the paragraph; a wrapped
            // continuation at its first glyph. The last line is extended to
            // the paragraph end below.
            let from = if visual.is_empty() || first == usize::MAX {
                start
            } else {
                start + first
            };
            let to = if last == 0 {
                start + run.text.len()
            } else {
                start + last
            };
            let line_width = run.line_w + pen_delta;
            let stops = caret_stops(text, from, to, run.rtl, &cells);
            width = width.max(line_width);
            height = height.max(run.line_top + run.line_height);
            visual.push(TextLine {
                range: from..to,
                top: run.line_top,
                height: run.line_height,
                width: line_width,
                stops,
                cells,
            });
        }

        // A paragraph the shaper produced no run for (an empty line) still
        // has a caret position: give it the previous line's metrics.
        let mut previous: Option<(f32, f32)> = None;
        let mut finished = Vec::new();
        for (i, mut visual) in paragraphs.into_iter().enumerate() {
            let end = line_starts.get(i + 1).map_or(text.len(), |next| next - 1);
            if visual.is_empty() {
                let (top, line_height) = previous.map_or((0.0, 0.0), |(top, h)| (top + h, h));
                let start = line_starts[i];
                height = height.max(top + line_height);
                visual.push(TextLine {
                    range: start..end.max(start),
                    top,
                    height: line_height,
                    width: 0.0,
                    stops: vec![CaretStop {
                        byte: start,
                        x: 0.0,
                    }],
                    cells: Vec::new(),
                });
            } else {
                // Soft wraps: each line ends where the next begins. The shaper
                // drops the whitespace it wraps at, so a line's glyphs may
                // stop short of that (the caret after them stands for the
                // gap). When they reach it exactly, the closing stop is the
                // next line's first byte; it is dropped so a click past this
                // line's end stays on it.
                for k in 0..visual.len() - 1 {
                    let next_start = visual[k + 1].range.start;
                    let line = &mut visual[k];
                    if line.range.end == next_start && line.stops.len() > 1 {
                        line.stops.pop();
                    }
                    line.range.end = next_start;
                }
                if let Some(last) = visual.last_mut() {
                    last.range.end = end.max(last.range.start);
                }
            }
            if let Some(last) = visual.last() {
                previous = Some((last.top, last.height));
            }
            finished.extend(visual);
        }
        let lines = finished;

        Self {
            lines,
            size: (width, height),
        }
    }

    /// Divide every length by `factor` (physical to logical pixels).
    pub(in crate::renderer) fn unscaled(mut self, factor: f32) -> Self {
        let inv = 1.0 / factor;
        self.size = (self.size.0 * inv, self.size.1 * inv);
        for line in &mut self.lines {
            line.top *= inv;
            line.height *= inv;
            line.width *= inv;
            for stop in &mut line.stops {
                stop.x *= inv;
            }
            for cell in &mut line.cells {
                cell.1 *= inv;
                cell.2 *= inv;
            }
        }
        self
    }

    /// Width and height of the whole text, matching `Layer::get_text_size`.
    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    pub fn lines(&self) -> &[TextLine] {
        &self.lines
    }

    /// The line containing byte offset `byte`; the end of a line belongs to
    /// that line, so a caret after the last character stays on it.
    fn line_of(&self, byte: usize) -> &TextLine {
        self.lines
            .iter()
            .rev()
            .find(|line| line.range.start <= byte)
            .unwrap_or(&self.lines[0])
    }

    /// The caret rectangle (zero width) at byte offset `byte`, clamped to
    /// the text. A boundary inside a ligature is interpolated along its
    /// glyph; a boundary inside a grapheme cluster snaps to the cluster
    /// start.
    pub fn caret_rect(&self, byte: usize) -> TextRect {
        let line = self.line_of(byte);
        let x = line
            .stops
            .iter()
            .rev()
            .find(|stop| stop.byte <= byte)
            .map_or(0.0, |stop| stop.x);
        TextRect {
            x,
            y: line.top,
            width: 0.0,
            height: line.height,
        }
    }

    /// The byte offset of the caret stop nearest `point`. A point above or
    /// below the text clamps to the first or last line.
    pub fn hit_test(&self, point: (f32, f32)) -> usize {
        let line = self
            .lines
            .iter()
            .find(|line| point.1 < line.top + line.height)
            .unwrap_or_else(|| &self.lines[self.lines.len() - 1]);
        line.stops
            .iter()
            .min_by(|a, b| {
                (a.x - point.0)
                    .abs()
                    .total_cmp(&(b.x - point.0).abs())
                    .then(a.byte.cmp(&b.byte))
            })
            .map_or(line.range.start, |stop| stop.byte)
    }

    /// Rectangles covering the selection `range`, one per contiguous run of
    /// glyphs per line (a bidirectional selection can need several). The
    /// line break itself is not painted.
    pub fn selection_rects(&self, range: Range<usize>) -> Vec<TextRect> {
        let mut rects = Vec::new();
        if range.is_empty() {
            return rects;
        }
        for line in &self.lines {
            let lo = range.start.max(line.range.start);
            let hi = range.end.min(line.range.end);
            if lo >= hi {
                continue;
            }
            let mut spans: Vec<(f32, f32)> = Vec::new();
            for (bytes, x, width) in &line.cells {
                let from = lo.max(bytes.start);
                let to = hi.min(bytes.end);
                if from >= to {
                    continue;
                }
                let at = |byte: usize| line.stops_x(byte, bytes, *x, *width);
                let (a, b) = (at(from), at(to));
                spans.push((a.min(b), a.max(b)));
            }
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut merged: Vec<(f32, f32)> = Vec::new();
            for span in spans {
                match merged.last_mut() {
                    Some(last) if span.0 <= last.1 + 0.01 => last.1 = last.1.max(span.1),
                    _ => merged.push(span),
                }
            }
            rects.extend(merged.into_iter().map(|(left, right)| TextRect {
                x: left,
                y: line.top,
                width: right - left,
                height: line.height,
            }));
        }
        rects
    }
}

impl TextLine {
    /// X of byte offset `byte` inside the glyph cell `bytes` placed at `x`
    /// with `width`, using the grapheme-aware stops.
    fn stops_x(&self, byte: usize, bytes: &Range<usize>, x: f32, width: f32) -> f32 {
        self.stops
            .iter()
            .find(|stop| stop.byte == byte)
            .map(|stop| stop.x)
            .unwrap_or_else(|| {
                let span = (bytes.end - bytes.start).max(1) as f32;
                x + width * ((byte - bytes.start) as f32 / span)
            })
    }
}

/// Caret stops for one line: every grapheme boundary, interpolated along the
/// glyph that holds it (a ligature of several graphemes shares one glyph).
fn caret_stops(
    text: &str,
    line_start: usize,
    end: usize,
    rtl: bool,
    cells: &[(Range<usize>, f32, f32)],
) -> Vec<CaretStop> {
    let mut stops: Vec<CaretStop> = Vec::new();
    for (bytes, x, width) in cells {
        let cluster = &text[bytes.clone()];
        let graphemes: Vec<usize> = cluster.grapheme_indices(true).map(|(i, _)| i).collect();
        let count = graphemes.len().max(1);
        for (n, offset) in graphemes.iter().copied().enumerate() {
            let fraction = n as f32 / count as f32;
            let x = if rtl {
                x + width * (1.0 - fraction)
            } else {
                x + width * fraction
            };
            stops.push(CaretStop {
                byte: bytes.start + offset,
                x,
            });
        }
    }
    stops.sort_by_key(|stop| stop.byte);
    stops.dedup_by_key(|stop| stop.byte);

    // The end of the line: the far edge of the last glyph in reading order.
    let end_x = cells
        .iter()
        .max_by_key(|(bytes, ..)| bytes.end)
        .map_or(0.0, |(_, x, width)| if rtl { *x } else { x + width });
    let start_x = stops.first().map_or(if rtl { end_x } else { 0.0 }, |s| s.x);
    if stops.first().is_none_or(|s| s.byte != line_start) {
        stops.insert(
            0,
            CaretStop {
                byte: line_start,
                x: start_x,
            },
        );
    }
    stops.push(CaretStop {
        byte: end,
        x: end_x,
    });
    stops.dedup_by_key(|stop| stop.byte);
    stops
}
