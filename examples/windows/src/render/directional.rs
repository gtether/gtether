use bytemuck::NoUninit;
use gtether::render::attachment::{AttachmentDescriptor, AttachmentMap};
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::UniformSet;
use gtether::render::{FlatVertex, RenderTarget, RendererHandle};
use std::sync::Arc;
use vulkano::buffer::{BufferContents, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::descriptor_set::layout::DescriptorType;
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, BlendFactor, BlendOp, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;

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

#[derive(BufferContents, NoUninit, Default, Debug, Clone, Copy)]
#[repr(C)]
pub struct PointLight {
    pub position: [f32; 4],
    pub color: [f32; 3],
}

pub struct DirectionalRenderer {
    graphics: Arc<EngineGraphicsPipeline>,
    screen_buffer: Subbuffer<[FlatVertex]>,
    descriptor_set: EngineDescriptorSet,
}

impl DirectionalRenderer {
    fn new(
        renderer: &RendererHandle,
        subpass: &Subpass,
        attachments: &Arc<dyn AttachmentMap>,
        lights: Arc<UniformSet<PointLight, 8>>,
    ) -> Self {
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

        let layout = {
            let mut layout_create_info = PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages);
            layout_create_info.set_layouts[0].bindings
                .get_mut(&2).unwrap()
                .descriptor_type = DescriptorType::UniformBufferDynamic;
            PipelineLayout::new(
                target.device().vk_device().clone(),
                layout_create_info
                    .into_pipeline_layout_create_info(target.device().vk_device().clone())
                    .unwrap(),
            ).unwrap()
        };
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
        );

        let descriptor_set = EngineDescriptorSet::builder(target)
            .layout(descriptor_layout)
            .descriptor_source(0, Arc::new(AttachmentDescriptor::new(
                attachments.clone(),
                "color",
            )))
            .descriptor_source(1, Arc::new(AttachmentDescriptor::new(
                attachments.clone(),
                "normals",
            )))
            .descriptor_source(2, lights)
            .build().unwrap();

        Self {
            graphics,
            screen_buffer: FlatVertex::screen_buffer(target.device().memory_allocator().clone()),
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for DirectionalRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();

        builder
            .bind_pipeline_graphics(graphics.clone()).unwrap()
            .bind_vertex_buffers(0, self.screen_buffer.clone()).unwrap();

        let descriptor_sets = self.descriptor_set
            .descriptor_set_with_offsets(frame.index())
            .unwrap();
        for descriptor_set in descriptor_sets {
            builder
                .bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    graphics.layout().clone(),
                    0,
                    descriptor_set,
                ).unwrap()
                .draw(self.screen_buffer.len() as u32, 1, 0, 0).unwrap();
        }
    }
}

pub struct DirectionalRendererBootstrap {
    lights: Arc<UniformSet<PointLight, 8>>
}

impl DirectionalRendererBootstrap {
    #[inline]
    pub fn new(target: &Arc<dyn RenderTarget>, lights: impl IntoIterator<Item=PointLight>) -> Arc<Self> {
        Arc::new(Self {
            lights: Arc::new(UniformSet::new(
                target,
                lights,
            ).unwrap()),
        })
    }

    pub fn bootstrap(self: &Arc<DirectionalRendererBootstrap>)
            -> impl FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>) -> DirectionalRenderer {
        let self_clone = self.clone();
        move |renderer, subpass, attachments| {
            DirectionalRenderer::new(renderer, subpass, attachments, self_clone.lights.clone())
        }
    }

    #[inline]
    pub fn lights(&self) -> &Arc<UniformSet<PointLight, 8>> { &self.lights }
}