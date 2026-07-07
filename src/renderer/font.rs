//! [`Font`] — points `draw_text` at a specific font, either one already
//! known to the system/renderer or bytes/a file to load.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// A font source for `draw_text`. Whichever variant is used, the renderer
/// registers it with its `FontSystem` at most once and caches the result,
/// so passing the same `Font` on every call is cheap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Font {
    /// A font family name already known to the system (or previously
    /// registered via [`Font::Bytes`]/[`Font::File`]), e.g. `"monospace"`.
    Named(String),
    /// Font file bytes embedded in the binary (typically via
    /// `include_bytes!`). `'static` because the renderer keeps the bytes
    /// registered for its own lifetime.
    Bytes(&'static [u8]),
    /// A font file to load from disk.
    #[allow(dead_code)] // not exercised by the current demo
    File(PathBuf),
}

/// Manual `Hash`: `Bytes` hashes by pointer identity + length, not content
/// — a `draw_text` call on an `Automatic` layer re-hashes its `Font` every
/// frame (see `DrawCommand`'s `Hash` impl), and a real embedded font file
/// can be megabytes; hashing its full contents every frame would be a real
/// cost for no benefit (a given `&'static [u8]` from `include_bytes!`
/// always has the same address for the same call site).
impl Hash for Font {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Font::Named(name) => {
                state.write_u8(0);
                name.hash(state);
            }
            Font::Bytes(bytes) => {
                state.write_u8(1);
                bytes.as_ptr().hash(state);
                bytes.len().hash(state);
            }
            Font::File(path) => {
                state.write_u8(2);
                path.hash(state);
            }
        }
    }
}
