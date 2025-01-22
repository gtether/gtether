use gtether::render::attachment::AttachmentMap;
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::model::{Model, ModelVertexNormal};
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::Uniform;
use gtether::render::{Device, RendererHandle};
use gtether::resource::Resource;
use parry3d::na::Point3;
use parry3d::query::{Ray, RayCast};
use std::sync::Arc;
use std::thread;
use parking_lot::RwLock;
use parry3d::bounding_volume::Aabb;
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
use gtether::gui::input::{InputDelegate, InputDelegateEvent};
use crate::render_util::{Camera, ModelTransform, MN, VP};

#[derive(Debug)]
pub struct Tile {
    pos: glm::TVec2<usize>,
    offset: glm::TVec2<f32>,
    aabb: Aabb,
}

impl Tile {
    fn new(pos: glm::TVec2<usize>, offset: glm::TVec2<f32>) -> Self {
        let aabb = Aabb::new(
            Point3::new(offset.x, 0.0, offset.y),
            Point3::new(offset.x + 1.0, 0.2, offset.y + 1.0),
        );
        Self {
            pos,
            offset,
            aabb,
        }
    }
}

#[derive(Debug)]
pub struct Board {
    transform: Arc<Uniform<MN, ModelTransform>>,
    camera: Arc<Uniform<VP, Camera>>,
    size: glm::TVec2<usize>,
    tiles: Vec<Tile>,
    selected_pos: RwLock<Option<glm::TVec2<usize>>>,
}

impl Board {
    pub fn new(
        input: InputDelegate,
        transform: Arc<Uniform<MN, ModelTransform>>,
        camera: Arc<Uniform<VP, Camera>>,
        size: glm::TVec2<usize>,
    ) -> Arc<Self> {
        let base_offset = glm::vec2(
            -(size.x as f32 / 2.0),
            -(size.y as f32 / 2.0),
        );
        let tiles = (0..size.y).map(move |y| {
            (0..size.x).map(move |x| {
                Tile::new(
                    glm::vec2(x, y),
                    base_offset + glm::vec2(x as f32, y as f32),
                )
            })
        }).flatten().collect::<Vec<_>>();

        let board = Arc::new(Self {
            transform,
            camera,
            size,
            tiles,
            selected_pos: RwLock::new(None),
        });

        let input_board = board.clone();
        thread::spawn(move || input.start(|_, event| {
            match event {
                InputDelegateEvent::CursorMoved(pos) => {
                    let transform = input_board.transform.read();
                    let camera = input_board.camera.read();

                    // Y is flipped because real-space uses a flipped Y compared to Vulkan screen space
                    let screen_coords = glm::vec2(
                        pos.x as f32 / camera.extent.x * 2.0 - 1.0,
                        -(pos.y as f32 / camera.extent.y * 2.0 - 1.0),
                    );

                    let near_point = camera.projection.unproject_point(
                        &Point3::new(screen_coords.x, screen_coords.y, -1.0)
                    );
                    let far_point = camera.projection.unproject_point(
                        &Point3::new(screen_coords.x, screen_coords.y, 1.0)
                    );
                    let ray = Ray::new(near_point, (far_point - near_point).normalize())
                        .inverse_transform_by(&camera.view);

                    let tile = input_board.tiles.iter().find(|tile| {
                        tile.aabb.intersects_ray(&transform.transform, &ray, f32::MAX)
                    });

                    *input_board.selected_pos.write() = tile.map(|tile| tile.pos);
                },
                _ => {}
            }
        }));

        board
    }

    #[inline]
    pub fn tile(&self, pos: glm::TVec2<usize>) -> Option<&Tile> {
        if pos.x < self.size.x && pos.y < self.size.y {
            Some(self.tiles.get(pos.x + (pos.y * self.size.x))
                .expect(&format!(
                    "Tiles count ({}) does not match size ({} * {})",
                    self.tiles.len(),
                    self.size.x,
                    self.size.y,
                )))
        } else {
            None
        }
    }

    #[inline]
    pub fn selected_tile(&self) -> Option<&Tile> {
        let selected_pos = self.selected_pos.read();
        if let Some(selected_pos) = *selected_pos {
            self.tile(selected_pos)
        } else {
            None
        }
    }

    pub fn bootstrap_renderer(
        self: &Arc<Self>,
        model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
        model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    ) -> impl FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>) -> BoardRenderer {
        let self_clone = self.clone();
        let transform = self.transform.clone();
        let camera = self.camera.clone();
        move |renderer, subpass, _| {
            BoardRenderer::new(
                self_clone,
                transform,
                camera,
                model_tile,
                model_piece,
                renderer,
                subpass,
            )
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
    model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
    model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    device: Arc<Device>,
    graphics: Arc<EngineGraphicsPipeline>,
    instances: Subbuffer<[TileInstance]>,
    descriptor_set: EngineDescriptorSet,
}

impl BoardRenderer {
    fn new(
        board: Arc<Board>,
        transform: Arc<Uniform<MN, ModelTransform>>,
        camera: Arc<Uniform<VP, Camera>>,
        model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
        model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
        renderer: &RendererHandle,
        subpass: &Subpass,
    ) -> Self {
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

        let instances = board.tiles.iter()
            .map(|tile| {
                let color = if (tile.pos.x + tile.pos.y) % 2 == 0 {
                    glm::vec3(0.2, 0.2, 0.2)
                } else {
                    glm::vec3(0.8, 0.8, 0.8)
                };
                TileInstance {
                    offset: glm::vec3(tile.offset.x, 0.0, tile.offset.y),
                    color,
                }
            })
            .collect::<Vec<_>>();

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
            .descriptor_source(0, camera)
            .descriptor_source(1, transform)
            .build().unwrap();

        Self {
            board,
            model_tile,
            model_piece,
            device: target.device().clone(),
            graphics,
            instances,
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for BoardRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();
        let model_tile = self.model_tile.read();
        let model_piece = self.model_piece.read();

        builder
            .bind_pipeline_graphics(graphics.clone()).unwrap()
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                self.descriptor_set.descriptor_set(frame.index()).unwrap(),
            ).unwrap();
        model_tile.draw_instanced(builder, self.instances.clone()).unwrap();

        let piece_instances = self.board.selected_tile().into_iter()
            .map(|tile| {
                TileInstance {
                    offset: glm::vec3(tile.offset.x, 0.2, tile.offset.y),
                    color: glm::vec3(0.2, 0.6 , 0.8),
                }
            });

        if piece_instances.len() > 0 {
            let piece_instance_buffer = Buffer::from_iter(
                self.device.memory_allocator().clone(),
                BufferCreateInfo {
                    usage: BufferUsage::VERTEX_BUFFER,
                    ..Default::default()
                },
                AllocationCreateInfo {
                    memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                        | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                    ..Default::default()
                },
                piece_instances,
            ).unwrap();

            model_piece.draw_instanced(builder, piece_instance_buffer).unwrap();
        }
    }
}