//! Per-frame renderer counters and resource-byte accounting.
//!
//! Performance and memory targets need numbers that mean something, so these
//! count what the renderer decided to do, not driver residency: bytes are
//! allocation arithmetic (`width * height * bytes per pixel * samples`) for
//! the textures this crate creates. Geometry buffers and glyph atlases are
//! not included. [`RenderStats`] is rebuilt by every frame.

/// What one rendered frame did and what its targets cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    /// Live layers this frame.
    pub layers: u32,
    /// Layers drawn straight into the frame (no render target of their own).
    pub direct_layers: u32,
    /// Layers rendered to an offscreen texture (an effect, or
    /// `CachePolicy::Always`) and composited.
    pub group_layers: u32,
    /// Offscreen content passes recorded this frame.
    pub group_content_passes: u32,
    /// Offscreen effect passes recorded this frame.
    pub group_effect_passes: u32,
    /// Text batches (one glyphon renderer each) across all layers.
    pub text_batches: u32,
    /// Layers whose geometry and ops were rebuilt this frame.
    pub rebuilt_layers: u32,
    /// Allocation arithmetic for the textures this crate owns.
    pub bytes: ResourceBytes,
}

/// Bytes allocated per kind of render target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceBytes {
    /// The shared multisampled colour target of the frame pass.
    pub msaa_color: u64,
    /// The shared multisampled stencil. Assumes 1 byte per sample;
    /// a backend that emulates `Stencil8` may use more.
    pub msaa_stencil: u64,
    /// Offscreen textures of cached/effect layers (plus the 1x1
    /// placeholders of direct layers).
    pub group_textures: u64,
    /// Effect scratch: output and, for blur, the ping texture.
    pub effect_textures: u64,
}

impl ResourceBytes {
    pub fn total(&self) -> u64 {
        self.msaa_color + self.msaa_stencil + self.group_textures + self.effect_textures
    }
}
