use gtether::render::attachment::AttachmentMap;
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::model::{Model, ModelVertexNormal};
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::Uniform;
use gtether::render::{RenderTarget, RendererHandle};
use gtether::resource::Resource;
use std::sync::Arc;
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

use crate::render_util::{MN, VP};

#[derive(Debug)]
pub struct Board {
    size: glm::TVec2<usize>,
    model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
    mn: Arc<Uniform<MN>>,
    vp: Arc<Uniform<VP>>,
}

impl Board {
    pub fn new(
        target: &Arc<dyn RenderTarget>,
        model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
        size: glm::TVec2<usize>,
        vp: Arc<Uniform<VP>>,
    ) -> Self {
        let mn = Arc::new(Uniform::new(
            target,
            MN::default(),
        ).unwrap());

        Self {
            size,
            model_tile,
            mn,
            vp,
        }
    }

    pub fn bootstrap_renderer(self: &Arc<Self>) -> impl FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>) -> BoardRenderer {
        let self_clone = self.clone();
        move |renderer, subpass, _| {
            BoardRenderer::new(self_clone, renderer, subpass)
        }
    }
}

#[derive(BufferContents, Vertex)]
#[repr(C)]
struct TileInstance {
    #[format(R32G32B32_SFLOAT)]
    offset: glm::TVec3<f32>,
    #[format(R32G32B32_SFLOAT)]
    color: glm::TVec3<f32>,
}

mod board_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "assets/shaders/board.vert",
    }
}

mod board_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "assets/shaders/board.frag",
    }
}

pub struct BoardRenderer {
    board: Arc<Board>,
    graphics: Arc<EngineGraphicsPipeline>,
    instances: Subbuffer<[TileInstance]>,
    descriptor_set: EngineDescriptorSet,
}

impl BoardRenderer {
    fn new(board: Arc<Board>, renderer: &RendererHandle, subpass: &Subpass) -> Self {
        let target = renderer.target();

        let board_vert = board_vert::load(target.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let board_frag = board_frag::load(target.device().vk_device().clone())
            .expect("Failed to create fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some([ModelVertexNormal::per_vertex(), TileInstance::per_instance()]
            .definition(&board_vert.info().input_interface)
            .unwrap());

        let stages = [
            PipelineShaderStageCreateInfo::new(board_vert),
            PipelineShaderStageCreateInfo::new(board_frag),
        ];

        let layout = PipelineLayout::new(
            target.device().vk_device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(target.device().vk_device().clone())
                .unwrap(),
        ).unwrap();
        let descriptor_layout = layout.set_layouts().get(0).unwrap().clone();

        let create_info = GraphicsPipelineCreateInfo {
            stages: stages.into_iter().collect(),
            vertex_input_state,
            subpass: Some(subpass.clone().into()),
            depth_stencil_state: Some(DepthStencilState {
                depth: Some(DepthState::simple()),
                ..Default::default()
            }),
            color_blend_state: Some(ColorBlendState::with_attachment_states(
                subpass.num_color_attachments(),
                ColorBlendAttachmentState::default(),
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
            ViewportType::BottomLeft,
        );

        let instances = {
            let base_offset = glm::vec3(
                -(board.size.x as f32 / 2.0),
                0.0,
                -(board.size.y as f32 / 2.0),
            );
            let (width, height) = (board.size.x, board.size.y);
            (0..width).into_iter().map(move |x| {
                (0..height).into_iter().map(move |y| {
                    let color = if (x + y) % 2 == 0 {
                        glm::vec3(0.2, 0.2, 0.2)
                    } else {
                        glm::vec3(0.8, 0.8, 0.8)
                    };
                    TileInstance {
                        offset: base_offset + glm::vec3(x as f32, 0.0, y as f32),
                        color,
                    }
                })
            }).flatten().collect::<Vec<_>>()
        };

        let instances = Buffer::from_iter(
            target.device().memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                    | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            instances,
        ).unwrap();

        let descriptor_set = EngineDescriptorSet::builder(target)
            .layout(descriptor_layout)
            .descriptor_source(0, board.vp.clone())
            .descriptor_source(1, board.mn.clone())
            .build().unwrap();

        Self {
            board,
            graphics,
            instances,
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for BoardRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();
        let model_tile = self.board.model_tile.read();

        builder
            .bind_pipeline_graphics(graphics.clone()).unwrap()
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                self.descriptor_set.descriptor_set(frame.index()).unwrap(),
            ).unwrap();
        model_tile.draw_instanced(builder, self.instances.clone()).unwrap();
    }
}