#![feature(generic_const_exprs)]
#![feature(coerce_unsized)]
#![allow(dead_code)]
use std::iter;

use crate::shader::ENTRY_FS_MAIN;
use futures::executor::block_on;
use glam::{vec3a, vec4};
use wgpu::util::{BufferInitDescriptor, DeviceExt};
use winit::{
    event::*,
    event_loop::EventLoop,
    window::{Window, WindowBuilder},
};

// Include the bindings generated by build.rs.
mod shader;
mod test;

struct State<'a> {
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group0: shader::bind_groups::BindGroup0,
    bind_group1: shader::bind_groups::BindGroup1,
    vertex_buffer: wgpu::Buffer,
}

impl<'a> State<'a> {
    async fn new(window: &'a Window) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(window).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: wgpu::Features::TEXTURE_COMPRESSION_BC,
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .unwrap();

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps.formats[0];
        let config = surface
            .get_default_config(&adapter, size.width, size.height)
            .unwrap();
        surface.configure(&device, &config);

        // Use the generated bindings to create the pipeline.
        let shader = shader::create_shader_module(&device);
        let render_pipeline_layout = shader::create_pipeline_layout(&device);

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: shader::vertex_state(
                &shader,
                &shader::vs_main_entry(wgpu::VertexStepMode::Vertex),
            ),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: ENTRY_FS_MAIN,
                targets: &[Some(surface_format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        // Create a gradient texture.
        let texture = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::all(),
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &vec![
                [0, 0, 255, 255],
                [64, 0, 255, 255],
                [128, 0, 255, 255],
                [255, 0, 255, 255],
                [0, 64, 255, 255],
                [64, 64, 255, 255],
                [128, 64, 255, 255],
                [255, 64, 255, 255],
                [0, 128, 255, 255],
                [64, 128, 255, 255],
                [128, 128, 255, 255],
                [255, 128, 255, 255],
                [0, 255, 255, 255],
                [64, 255, 255, 255],
                [128, 255, 255, 255],
                [255, 255, 255, 255],
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>(),
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let rts_buffer = device.create_buffer_init(&BufferInitDescriptor{
          contents: &[1,2,3],
          label: Some("rts"),
          usage: wgpu::BufferUsages::all()
        });

        // Use the generated types to ensure the correct bind group is assigned to each slot.
        let bind_group0 = shader::bind_groups::BindGroup0::from_bindings(
            &device,
            shader::bind_groups::BindGroupLayout0 {
                color_texture: &view,
                color_sampler: &sampler,
                rts: rts_buffer.as_entire_buffer_binding()
            },
        );

        let uniforms_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uniforms"),
            contents: bytemuck::cast_slice(&[shader::Uniforms::new(vec4(1.0, 1.0, 1.0, 1.0))]),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let bind_group1 = shader::bind_groups::BindGroup1::from_bindings(
            &device,
            shader::bind_groups::BindGroupLayout1 {
                uniforms: uniforms_buffer.as_entire_buffer_binding(),
            },
        );

        // Initialize the vertex buffer based on the expected input structs.
        // For storage buffer compatibility, consider using encase instead.
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: bytemuck::cast_slice(&[
                shader::VertexInput {
                    position: vec3a(-1.0, -1.0, 0.0),
                },
                shader::VertexInput {
                    position: vec3a(3.0, -1.0, 0.0),
                },
                shader::VertexInput {
                    position: vec3a(-1.0, 3.0, 0.0),
                },
            ]),
            usage: wgpu::BufferUsages::VERTEX,
        });

        Self {
            surface,
            device,
            queue,
            size,
            config,
            pipeline,
            bind_group0,
            bind_group1,
            vertex_buffer,
        }
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let output_view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &output_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        render_pass.set_pipeline(&self.pipeline);

        // Use this function to ensure all bind groups are set.
        crate::shader::set_bind_groups(&mut render_pass, &self.bind_group0, &self.bind_group1);

        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.draw(0..3, 0..1);

        drop(render_pass);
        self.queue.submit(iter::once(encoder.finish()));

        // Actually draw the frame.
        output.present();

        Ok(())
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new()
        .with_title("wgsl_bindgen example")
        .build(&event_loop)
        .unwrap();

    let mut state = block_on(State::new(&window));
    event_loop
        .run(|event, target| match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => target.exit(),
                WindowEvent::Resized(physical_size) => {
                    state.resize(*physical_size);
                    window.request_redraw();
                }
                WindowEvent::ScaleFactorChanged { .. } => {}
                WindowEvent::RedrawRequested => {
                    match state.render() {
                        Ok(_) => {}
                        Err(wgpu::SurfaceError::Lost) => state.resize(state.size),
                        Err(wgpu::SurfaceError::OutOfMemory) => target.exit(),
                        Err(e) => eprintln!("{e:?}"),
                    }
                    window.request_redraw();
                }
                _ => {
                    window.request_redraw();
                }
            },
            _ => (),
        })
        .unwrap();
}
