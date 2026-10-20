//! [`Pixels`] — a raw RGBA8 image buffer for `draw_image`.

/// A raw RGBA8 pixel buffer, row-major, no padding between rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    /// Length must be exactly `width * height * 4`.
    pub data: Vec<u8>,
}

impl Pixels {
    /// Wrap a raw RGBA8 buffer. Panics if `data.len()` doesn't match
    /// `width * height * 4` — every renderer entry point that consumes
    /// pixel data (e.g. `queue.write_texture`) requires that invariant to
    /// already hold, so it's cheaper and clearer to check it once here.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert_eq!(
            data.len(),
            width as usize * height as usize * 4,
            "Pixels data length must be width * height * 4"
        );
        Self {
            width,
            height,
            data,
        }
    }
}
