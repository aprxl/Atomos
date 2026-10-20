//! Headless GPU harness for pixel regression tests.
//!
//! [`Scene`] stands in for [`super::Renderer`]'s window plumbing: a fixed
//! `Rgba8UnormSrgb` target cleared to opaque black, 4x MSAA, and the real
//! layer pipeline from [`super::layer::render_layers`]. Tests draw through
//! ordinary [`Layer`] handles and read single pixels back.
//!
//! [`Scene::new`] returns `None` when the machine has no usable adapter so
//! the suite still passes on GPU-less CI; tests `return` in that case.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use super::layer::{self, LayerInner, LayerPipelines};
use super::text_stack::TextStack;
use super::{Layer, LayerInvalidation};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const MSAA: u32 = 4;

pub(super) struct Scene {
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: (u32, u32),
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    msaa: layer::MsaaTarget,
    pipelines: LayerPipelines,
    text: Rc<RefCell<TextStack>>,
    order: VecDeque<Weak<RefCell<LayerInner>>>,
}

impl Scene {
    pub(super) fn new(size: (u32, u32)) -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("atomos-test-device"),
            ..Default::default()
        }))
        .ok()?;

        let pipelines = LayerPipelines::new(&device, FORMAT, MSAA);
        let text = Rc::new(RefCell::new(TextStack::new(
            &device, &queue, FORMAT, size, MSAA,
        )));
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atomos-test-target"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let msaa = layer::create_msaa_target(&device, FORMAT, size, MSAA);
        Some(Self {
            device,
            queue,
            size,
            target,
            target_view,
            msaa,
            pipelines,
            text,
            order: VecDeque::new(),
        })
    }

    pub(super) fn text(&self) -> std::cell::RefMut<'_, TextStack> {
        self.text.borrow_mut()
    }

    /// A new top layer; `Manual` so tests control when content changes.
    pub(super) fn layer(&mut self) -> Layer {
        let layer = Layer::new(
            &self.device,
            &self.queue,
            &self.pipelines,
            self.text.clone(),
            FORMAT,
            self.size,
            1.0,
            LayerInvalidation::Manual,
        );
        self.order.push_back(Rc::downgrade(&layer.0));
        layer
    }

    /// Clear the target to opaque black, render every layer onto it, and
    /// wait for the GPU.
    pub(super) fn frame(&mut self, scale_factor: f32) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        }));
        self.text.borrow_mut().advance_frame();
        layer::render_layers(
            &mut encoder,
            &self.target_view,
            &self.msaa,
            &mut self.order,
            self.size,
            scale_factor,
        );
        self.queue.submit([encoder.finish()]);
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU poll failed");
    }

    /// The sRGB-encoded RGBA bytes at `(x, y)` of the last frame.
    pub(super) fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("atomos-test-readback"),
            size: 256,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                origin: wgpu::Origin3d { x, y, z: 0 },
                ..self.target.as_image_copy()
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                tx.send(result).expect("readback receiver dropped")
            });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU poll failed");
        rx.recv()
            .expect("readback sender dropped")
            .expect("map failed");
        let bytes = buffer.slice(..).get_mapped_range();
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }
}
