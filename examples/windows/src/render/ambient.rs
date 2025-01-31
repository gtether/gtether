use std::sync::Arc;

use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::frame::FrameManagerExt;
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::uniform::Uniform;
use gtether::render::{FlatVertex, Renderer, VulkanoError};
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
use vulkano::Validated;

mod ambient_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "src/shaders/ambient.vert",
    }
}

mod ambient_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "src/shaders/ambient.frag",
    }
}

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct AmbientLight {
    pub color: [f32; 3],
    pub intensity: f32,
}

impl AmbientLight {
    #[inline]
    pub fn new(r: f32, g: f32, b: f32, intensity: f32) -> Self {
        Self {
            color: [r, g, b],
            intensity,
        }
    }
}

impl Default for AmbientLight {
    #[inline]
    fn default() -> Self {
        Self::new(1.0, 1.0, 1.0, 0.1)
    }
}

pub struct AmbientRenderer {
    graphics: Arc<EngineGraphicsPipeline>,
    screen_buffer: Subbuffer<[FlatVertex]>,
    descriptor_set: EngineDescriptorSet,
}

impl AmbientRenderer {
    fn new(
        renderer: &Arc<Renderer>,
        subpass: &Subpass,
        light: Arc<Uniform<AmbientLight>>
    ) -> Self {
        let ambient_vert = ambient_vert::load(renderer.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let ambient_frag = ambient_frag::load(renderer.device().vk_device().clone())
            .expect("Failed to create fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some(FlatVertex::per_vertex()
            .definition(&ambient_vert.info().input_interface)
            .unwrap());

        let stages = [
            PipelineShaderStageCreateInfo::new(ambient_vert),
            PipelineShaderStageCreateInfo::new(ambient_frag),
        ];

        let layout = PipelineLayout::new(
            renderer.device().vk_device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(renderer.device().vk_device().clone())
                .unwrap(),
        ).unwrap();
        let descriptor_layout = layout.set_layouts().get(0).unwrap().clone();

        let create_info = GraphicsPipelineCreateInfo {
            stages: stages.into_iter().collect(),
            vertex_input_state,
            subpass: Some(subpass.clone().into()),
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
            create_info,
            ViewportType::TopLeft,
        );

        let descriptor_set = EngineDescriptorSet::builder(renderer.clone())
            .layout(descriptor_layout)
            .descriptor_source(0, renderer.frame_manager().attachment_descriptor("color"))
            .descriptor_source(1, light)
            .build();

        Self {
            graphics,
            screen_buffer: FlatVertex::screen_buffer(renderer.device().memory_allocator().clone()),
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for AmbientRenderer {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        let graphics = self.graphics.vk_graphics();

        builder
            .bind_pipeline_graphics(graphics.clone())?
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                self.descriptor_set.descriptor_set().map_err(VulkanoError::from_validated)?,
            )?
            .bind_vertex_buffers(0, self.screen_buffer.clone())?
            .draw(self.screen_buffer.len() as u32, 1, 0, 0)?;
        Ok(())
    }
}

pub struct AmbientRendererBootstrap {
    light: Arc<Uniform<AmbientLight>>,
}

impl AmbientRendererBootstrap {
    #[inline]
    pub fn new(renderer: &Arc<Renderer>) -> Arc<Self> {
        Arc::new(Self {
            light: Arc::new(Uniform::new(
                renderer.device().clone(),
                renderer.frame_manager(),
                AmbientLight::default(),
            ).unwrap()),
        })
    }

    pub fn bootstrap(self: &Arc<AmbientRendererBootstrap>)
            -> impl FnOnce(&Arc<Renderer>, &Subpass) -> AmbientRenderer {
        let self_clone = self.clone();
        move |renderer, subpass| {
            AmbientRenderer::new(renderer, subpass, self_clone.light.clone())
        }
    }

    #[inline]
    pub fn light(&self) -> &Arc<Uniform<AmbientLight>> { &self.light }
}