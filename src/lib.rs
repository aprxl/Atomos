//! Atomos — a native application backend: rendering, layout, styling, input
//! and output on wgpu + winit, consumed as a library.
//!
//! Applications own their winit event loop and drive these pieces:
//! [`renderer::Renderer`] draws, [`input::Input`] gathers window events,
//! [`frame::FrameScheduler`] keeps the loop demand-driven, and
//! [`animation`] supplies timed values.

pub mod animation;
pub mod frame;
pub mod input;
pub mod renderer;
