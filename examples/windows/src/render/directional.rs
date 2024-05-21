use std::sync::Arc;
use vulkano::buffer::{BufferContents, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::pipeline::{GraphicsPipeline, Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, BlendFactor, BlendOp, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::viewport::{Viewport, ViewportState};
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::render_pass::Subpass;
use gtether::render::render_pass::{AttachmentMap, EngineRenderHandler};
use gtether::render::RenderTarget;
use gtether::render::swapchain::Framebuffer;
use crate::render::{AttachmentSet, FlatVertex, UniformSet};

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
    graphics: Option<Arc<GraphicsPipeline>>,
    screen_buffer: Subbuffer<[FlatVertex]>,
    attachments: AttachmentSet,
    lights: Arc<UniformSet<PointLight>>,
}

impl DirectionalRenderer {
    pub fn new(target: &Arc<dyn RenderTarget>) -> Self {
        Self {
            target: target.clone(),
            graphics: None,
            screen_buffer: FlatVertex::screen_buffer(target),
            attachments: AttachmentSet::new(target, 0),
            lights: Arc::new(UniformSet::new(vec![], target, 1)),
        }
    }

    #[inline]
    pub fn lights(&self) -> &Arc<UniformSet<PointLight>> { &self.lights }
}

impl EngineRenderHandler for DirectionalRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.as_ref()
            .expect(".recreate() not called yet");

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

    fn recreate(&mut self, target: &Arc<dyn RenderTarget>, subpass: &Subpass, attachments: &Arc<dyn AttachmentMap>) {
        self.target = target.clone();

        let viewport = Viewport {
            offset: [0.0, 0.0],
            extent: target.dimensions().into(),
            depth_range: 0.0..=1.0,
        };

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

        let graphics = GraphicsPipeline::new(
            target.device().vk_device().clone(),
            None,
            GraphicsPipelineCreateInfo {
                viewport_state: Some(ViewportState {
                    viewports: [viewport].into_iter().collect(),
                    ..Default::default()
                }),
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
                subpass: Some(subpass.clone().into()),
                stages: stages.into_iter().collect(),
                vertex_input_state,
                input_assembly_state: Some(InputAssemblyState::default()),
                rasterization_state: Some(RasterizationState {
                    cull_mode: CullMode::Back,
                    ..Default::default()
                }),
                multisample_state: Some(MultisampleState::default()),
                ..GraphicsPipelineCreateInfo::layout(layout)
            }
        ).unwrap();

        self.attachments.recreate(
            target,
            &graphics,
            attachments,
            &["color".into(), "normals".into()],
        );
        self.lights.recreate(target, &graphics);
        self.graphics = Some(graphics);
    }
}