use std::sync::Arc;

use gtether::render::attachment::{AttachmentMap, AttachmentSet};
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::UniformSet;
use gtether::render::{RenderTarget, RendererHandle};
use vulkano::buffer::{BufferContents, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, BlendFactor, BlendOp, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;

use crate::render::FlatVertex;

mod directional_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "src/shaders/directional.vert",
    }
}

mod directional_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "src/shaders/directional.frag",
    }
}

#[derive(BufferContents, Default, Debug, Clone)]
#[repr(C)]
pub struct PointLight {
    pub position: [f32; 4],
    pub color: [f32; 3],
}

pub struct DirectionalRenderer {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<EngineGraphicsPipeline>,
    screen_buffer: Subbuffer<[FlatVertex]>,
    attachments: Arc<AttachmentSet>,
    lights: Arc<UniformSet<PointLight>>,
}

impl DirectionalRenderer {
    pub fn new(renderer: &RendererHandle) -> Self {
        let target = renderer.target();

        let directional_vert = directional_vert::load(target.device().vk_device().clone())
            .expect("Failed to create directional vertex shader module")
            .entry_point("main").unwrap();

        let directional_frag = directional_frag::load(target.device().vk_device().clone())
            .expect("Failed to create directional fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some(FlatVertex::per_vertex()
            .definition(&directional_vert.info().input_interface)
            .unwrap());

        let stages = [
            PipelineShaderStageCreateInfo::new(directional_vert),
            PipelineShaderStageCreateInfo::new(directional_frag),
        ];

        let layout = PipelineLayout::new(
            target.device().vk_device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(target.device().vk_device().clone())
                .unwrap(),
        ).unwrap();

        let base_create_info = GraphicsPipelineCreateInfo {
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
                    ColorBlendAttachmentState {
                        blend: Some(AttachmentBlend {
                            src_color_blend_factor: BlendFactor::One,
                            dst_color_blend_factor: BlendFactor::One,
                            color_blend_op: BlendOp::Add,
                            src_alpha_blend_factor: BlendFactor::One,
                            dst_alpha_blend_factor: BlendFactor::One,
                            alpha_blend_op: BlendOp::Max,
                        }),
                        ..Default::default()
                    },
                )),
                ..base_create_info.clone()
            },
        );

        let attachments = AttachmentSet::new(
            renderer,
            graphics.clone(),
            vec!["color".into(), "normals".into()],
            0,
        );

        let lights = UniformSet::new(
            vec![],
            renderer,
            graphics.clone(),
            1,
        );

        Self {
            target: target.clone(),
            graphics,
            screen_buffer: FlatVertex::screen_buffer(target),
            attachments,
            lights,
        }
    }

    #[inline]
    pub fn lights(&self) -> &Arc<UniformSet<PointLight>> { &self.lights }
}

impl EngineRenderHandler for DirectionalRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();

        for light_descriptor_set in self.lights.descriptor_sets() {
            builder
                .bind_pipeline_graphics(graphics.clone()).unwrap()
                .bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    graphics.layout().clone(),
                    0,
                    (
                        self.attachments.descriptor_set(frame.index()).unwrap().clone(),
                        light_descriptor_set.clone(),
                    ),
                ).unwrap()
                .bind_vertex_buffers(0, self.screen_buffer.clone()).unwrap()
                .draw(self.screen_buffer.len() as u32, 1, 0, 0).unwrap();
        }
    }

    fn init(&mut self, target: &Arc<dyn RenderTarget>, subpass: &Subpass, attachments: &Arc<dyn AttachmentMap>) {
        self.target = target.clone();
        self.graphics.init(subpass.clone());
        self.attachments.init(attachments.clone());
    }
}