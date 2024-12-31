use std::sync::Arc;

use gtether::event::Event;
use gtether::render::attachment::AttachmentMap;
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::Uniform;
use gtether::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};
use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::graphics::color_blend::{ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::depth_stencil::{DepthState, DepthStencilState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;

use crate::render::{MN, VP};

mod deferred_vert {
    vulkano_shaders::shader! {
        ty:  "vertex",
        path: "src/shaders/deferred.vert",
    }
}

mod deferred_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "src/shaders/deferred.frag",
    }
}

#[derive(BufferContents, Vertex)]
#[repr(C)]
struct CubeVertex {
    #[format(R32G32B32_SFLOAT)]
    position: [f32; 3],
    #[format(R32G32B32_SFLOAT)]
    normal: [f32; 3],
    #[format(R32G32B32_SFLOAT)]
    color: [f32; 3],
}

pub struct CubeRenderer {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<EngineGraphicsPipeline>,
    vertex_buffer: Subbuffer<[CubeVertex]>,
    mn: Arc<Uniform<MN>>,
    vp: Arc<Uniform<VP>>,
}

impl CubeRenderer {
    pub fn new(renderer: &RendererHandle) -> Self {
        let target = renderer.target();

        let color = [1.0, 1.0, 1.0];

        let vertices = [
            // Front
            CubeVertex { position: [-1.0, -1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            CubeVertex { position: [-1.0,  1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            CubeVertex { position: [ 1.0,  1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            CubeVertex { position: [-1.0, -1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            CubeVertex { position: [ 1.0,  1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            CubeVertex { position: [ 1.0, -1.0,  1.0], normal: [0.0,  0.0,  1.0], color },
            // Back
            CubeVertex { position: [ 1.0, -1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            CubeVertex { position: [ 1.0,  1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            CubeVertex { position: [-1.0,  1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            CubeVertex { position: [ 1.0, -1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            CubeVertex { position: [-1.0,  1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            CubeVertex { position: [-1.0, -1.0, -1.0], normal: [ 0.0,  0.0, -1.0], color },
            // Top
            CubeVertex { position: [-1.0, -1.0,  1.0], normal: [ 0.0, -1.0,  0.0], color },
            CubeVertex { position: [ 1.0, -1.0,  1.0], normal: [ 0.0, -1.0,  0.0], color },
            CubeVertex { position: [ 1.0, -1.0, -1.0], normal: [ 0.0, -1.0,  0.0], color },
            CubeVertex { position: [-1.0, -1.0,  1.0], normal: [ 0.0, -1.0,  0.0], color },
            CubeVertex { position: [ 1.0, -1.0, -1.0], normal: [ 0.0, -1.0,  0.0], color },
            CubeVertex { position: [-1.0, -1.0, -1.0], normal: [ 0.0, -1.0,  0.0], color },
            // Bottom
            CubeVertex { position: [ 1.0,  1.0,  1.0], normal: [ 0.0,  1.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0,  1.0], normal: [ 0.0,  1.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0, -1.0], normal: [ 0.0,  1.0,  0.0], color },
            CubeVertex { position: [ 1.0,  1.0,  1.0], normal: [ 0.0,  1.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0, -1.0], normal: [ 0.0,  1.0,  0.0], color },
            CubeVertex { position: [ 1.0,  1.0, -1.0], normal: [ 0.0,  1.0,  0.0], color },
            // Left
            CubeVertex { position: [-1.0, -1.0, -1.0], normal: [-1.0,  0.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0, -1.0], normal: [-1.0,  0.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0,  1.0], normal: [-1.0,  0.0,  0.0], color },
            CubeVertex { position: [-1.0, -1.0, -1.0], normal: [-1.0,  0.0,  0.0], color },
            CubeVertex { position: [-1.0,  1.0,  1.0], normal: [-1.0,  0.0,  0.0], color },
            CubeVertex { position: [-1.0, -1.0,  1.0], normal: [-1.0,  0.0,  0.0], color },
            // Right
            CubeVertex { position: [ 1.0, -1.0,  1.0], normal: [ 1.0,  0.0,  0.0], color },
            CubeVertex { position: [ 1.0,  1.0,  1.0], normal: [ 1.0,  0.0,  0.0], color },
            CubeVertex { position: [ 1.0,  1.0, -1.0], normal: [ 1.0,  0.0,  0.0], color },
            CubeVertex { position: [ 1.0, -1.0,  1.0], normal: [ 1.0,  0.0,  0.0], color },
            CubeVertex { position: [ 1.0,  1.0, -1.0], normal: [ 1.0,  0.0,  0.0], color },
            CubeVertex { position: [ 1.0, -1.0, -1.0], normal: [ 1.0,  0.0,  0.0], color },
        ];

        let vertex_buffer = Buffer::from_iter(
            target.device().memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            vertices,
        ).unwrap();

        let deferred_vert = deferred_vert::load(target.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let deferred_frag = deferred_frag::load(target.device().vk_device().clone())
            .expect("Failed to create fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some(CubeVertex::per_vertex()
            .definition(&deferred_vert.info().input_interface)
            .unwrap());

        let stages = [
            PipelineShaderStageCreateInfo::new(deferred_vert),
            PipelineShaderStageCreateInfo::new(deferred_frag),
        ];

        let layout = PipelineLayout::new(
            target.device().vk_device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(target.device().vk_device().clone())
                .unwrap(),
        ).unwrap();

        let base_create_info = GraphicsPipelineCreateInfo {
            depth_stencil_state: Some(DepthStencilState {
                depth: Some(DepthState::simple()),
                ..Default::default()
            }),
            stages: stages.into_iter().collect(),
            vertex_input_state,
            input_assembly_state: Some(InputAssemblyState::default()),
            rasterization_state: Some(RasterizationState {
                cull_mode: CullMode::Back,
                ..Default::default()
            }),
            multisample_state: Some(MultisampleState::default()),
            ..GraphicsPipelineCreateInfo::layout(layout)
        };

        let graphics = EngineGraphicsPipeline::new(
            renderer,
            move |subpass| GraphicsPipelineCreateInfo {
                color_blend_state: Some(ColorBlendState::with_attachment_states(
                    subpass.num_color_attachments(),
                    ColorBlendAttachmentState::default(),
                )),
                ..base_create_info.clone()
            },
        );

        let mn = Uniform::new(
            MN::default(),
            renderer,
            graphics.clone(),
            1,
        );

        let vp = Uniform::new(
            VP::default(),
            renderer,
            graphics.clone(),
            0,
        );

        let reset_vp = vp.clone();
        renderer.event_bus().register(
            RendererEventType::Stale,
            move |event: &mut Event<RendererEventType, RendererEventData>| {
                reset_vp.write().projection = glm::perspective(
                    event.target().dimensions().aspect_ratio(),
                    glm::half_pi(),
                    0.01, 100.0,
                );
            }
        );

        Self {
            target: target.clone(),
            graphics,
            vertex_buffer,
            mn,
            vp,
        }
    }

    #[inline]
    pub fn mn(&self) -> &Arc<Uniform<MN>> { &self.mn }

    #[inline]
    pub fn vp(&self) -> &Arc<Uniform<VP>> { &self.vp }
}

impl EngineRenderHandler for CubeRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, _frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();

        builder
            .bind_pipeline_graphics(graphics.clone()).unwrap()
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                (
                    self.vp.descriptor_set().clone(),
                    self.mn.descriptor_set().clone(),
                ),
            ).unwrap()
            .bind_vertex_buffers(0, self.vertex_buffer.clone()).unwrap()
            .draw(self.vertex_buffer.len() as u32, 1, 0, 0).unwrap();
    }

    fn init(&mut self, target: &Arc<dyn RenderTarget>, subpass: &Subpass, _attachments: &Arc<dyn AttachmentMap>) {
        self.target = target.clone();
        self.graphics.init(subpass.clone());
    }
}