use bitcode::{Decode, Encode};
use gtether::client::gui::input::{ElementState, InputDelegate, InputDelegateEvent, InputDelegateJoinHandle, MouseButton};
use gtether::event::{Event, EventHandler};
use gtether::net::gns::GnsClientDriver;
use gtether::net::message::MessageHandler;
use gtether::net::message::{Message, MessageBody};
use gtether::net::{Connection, Networking};
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::font::compositor::FontCompositor;
use gtether::render::font::layout::{LayoutAlignment, LayoutHorizontalAlignment, LayoutVerticalAlignment, TextLayout, TextLayoutCreateInfo};
use gtether::render::font::sheet::{FontSheet, FontSheetRenderer, UnicodeFontSheetMap};
use gtether::render::font::size::{FontSize, FontSizer};
use gtether::render::font::Font;
use gtether::render::model::{Model, ModelVertexNormal};
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::uniform::Uniform;
use gtether::render::{EngineDevice, RenderTarget, Renderer, RendererStaleEvent, VulkanoError};
use gtether::resource::Resource;
use parking_lot::{Mutex, RwLock};
use parry3d::bounding_volume::Aabb;
use parry3d::na::Point3;
use parry3d::query::{Ray, RayCast};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use tracing::warn;
use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::depth_stencil::{DepthState, DepthStencilState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;
use vulkano::Validated;

use crate::board::controller::MessagePlay;
use crate::board::{BoardState, GameState, Tile};
use crate::render_util::{Camera, ModelTransform, MN, VP};

#[derive(Debug, Clone)]
pub struct TileView {
    tile: Tile,
    offset: glm::TVec2<f32>,
    aabb: Aabb,
}

impl TileView {
    fn new(tile: Tile, offset: glm::TVec2<f32>) -> Self {
        let aabb = Aabb::new(
            Point3::new(offset.x, 0.0, offset.y),
            Point3::new(offset.x + 1.0, 0.2, offset.y + 1.0),
        );
        Self {
            tile,
            offset,
            aabb,
        }
    }
}

#[derive(Encode, Decode, MessageBody, Debug)]
#[message_flag(Reliable)]
pub struct MessageUpdateBoard {
    board: BoardState,
}

impl MessageUpdateBoard {
    #[inline]
    pub fn new(
        board: BoardState,
    ) -> Self {
        Self {
            board,
        }
    }

    #[inline]
    pub fn into_board_state(self) -> BoardState {
        self.board
    }
}

#[derive(Debug)]
struct BoardViewState {
    board: BoardState,
    tiles: Vec<TileView>,
    selected_pos: Option<glm::TVec2<usize>>,
}

impl BoardViewState {
    fn selected_tile(&self) -> Option<&TileView> {
        if let Some(pos) = self.selected_pos {
            let size = self.board.size();
            self.tiles.get(pos.x + (pos.y * size.x))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct BoardView {
    state: Arc<RwLock<BoardViewState>>,
    local_players: HashSet<usize>,
    transform: Arc<Uniform<MN, ModelTransform>>,
    camera: Arc<Uniform<VP, Camera>>,
    input_join_handle: Option<InputDelegateJoinHandle>,
}

impl BoardView {
    fn create_tile_views(board: &BoardState) -> Vec<TileView> {
        let size = board.size();
        let base_offset = glm::vec2(
            -(size.x as f32 / 2.0),
            -(size.y as f32 / 2.0),
        );
        board.iter()
            .map(|tile| {
                let pos = tile.pos();
                TileView::new(
                    tile.clone(),
                    base_offset + glm::vec2(pos.x as f32, pos.y as f32),
                )
            })
            .collect::<Vec<_>>()
    }

    pub fn new(
        board: BoardState,
        input: InputDelegate,
        net: Arc<Networking<GnsClientDriver>>,
        local_players: HashSet<usize>,
        transform: Arc<Uniform<MN, ModelTransform>>,
        camera: Arc<Uniform<VP, Camera>>,
    ) -> Arc<Self> {
        let tiles = Self::create_tile_views(&board);
        let state = Arc::new(RwLock::new(BoardViewState {
            board,
            tiles,
            selected_pos: None,
        }));

        let input_state = state.clone();
        let input_local_players = local_players.clone();
        let input_transform = transform.clone();
        let input_camera = camera.clone();
        let input_net = net.clone();
        let input_join_handle = input.spawn(move |_, event| {
            match event {
                InputDelegateEvent::CursorMoved(pos) => {
                    let transform = input_transform.read();
                    let camera = input_camera.read();

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

                    let mut state = input_state.write();
                    let tile_pos = state.tiles.iter().find(|tile| {
                        tile.aabb.intersects_ray(&transform.transform, &ray, f32::MAX)
                    }).map(|tile| tile.tile.pos());

                    state.selected_pos = None;
                    if input_local_players.contains(&state.board.current_player_idx()) {
                        if let Some(pos) = tile_pos {
                            if state.board.valid_selection(pos) {
                                state.selected_pos = Some(pos);
                            }
                        }
                    }
                },
                InputDelegateEvent::MouseButton(event) => {
                    if event.button == MouseButton::Left && event.state == ElementState::Pressed {
                        let selected_pos = {
                            let state = input_state.read();
                            if input_local_players.contains(&state.board.current_player_idx()) {
                                state.selected_pos
                                    .map(|pos| (pos, state.board.current_player_idx()))
                            } else {
                                None
                            }
                        };

                        if let Some((pos, player_idx)) = selected_pos {
                            let msg = MessagePlay::new(player_idx, pos);
                            match input_net.send(msg) {
                                Ok(_) => {},
                                Err(error) =>
                                    warn!(?error, "Failed to send MessagePlay"),
                            }
                        }
                    }
                },
                _ => {}
            }

        });

        let board_view = Arc::new(Self {
            state,
            local_players,
            transform,
            camera,
            input_join_handle: Some(input_join_handle),
        });

        net.insert_msg_handler(Arc::downgrade(&board_view));

        board_view
    }

    pub fn bootstrap_renderer(
        self: &Arc<Self>,
        model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
        model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    ) -> impl FnOnce(&Arc<Renderer>, &Subpass) -> BoardRenderer {
        let board_view = self.clone();
        let transform = self.transform.clone();
        let camera = self.camera.clone();
        move |renderer, subpass| {
            BoardRenderer::new(
                board_view,
                transform,
                camera,
                model_tile,
                model_piece,
                renderer,
                subpass,
            )
        }
    }

    pub fn bootstrap_text_renderer(
        self: &Arc<Self>,
        font: Arc<Resource<dyn Font>>,
    ) -> impl FnOnce(&Arc<Renderer>, &Subpass) -> BoardTextRenderer {
        let self_clone = self.clone();
        move |renderer, subpass| {
            BoardTextRenderer::new(
                renderer,
                subpass,
                font,
                self_clone,
            )
        }
    }
}

impl Drop for BoardView {
    fn drop(&mut self) {
        if let Some(join_handle) = self.input_join_handle.take() {
            join_handle.join().unwrap();
        }
    }
}

impl MessageHandler<MessageUpdateBoard, Infallible> for BoardView {
    fn handle(&self, _connection: Connection, msg: Message<MessageUpdateBoard>) -> Result<(), Infallible> {
        let mut state = self.state.write();

        let board = msg.into_body().into_board_state();
        if board.version() > state.board.version() {
            if state.selected_pos.is_some() {
                if board.size() != state.board.size() {
                    state.selected_pos = None;
                } else if board.tile(state.selected_pos.unwrap()).unwrap().owner.is_some() {
                    state.selected_pos = None;
                } else if !self.local_players.contains(&board.current_player_idx()) {
                    state.selected_pos = None;
                }
            }

            state.board = board;
            state.tiles = Self::create_tile_views(&state.board);
        }

        Ok(())
    }

    #[inline]
    fn is_valid(&self) -> bool {
        true
    }
}

#[derive(BufferContents, Vertex)]
#[repr(C)]
struct TileInstance {
    #[format(R32G32B32_SFLOAT)]
    offset: glm::TVec3<f32>,
    #[format(R32G32B32A32_SFLOAT)]
    color: glm::TVec4<f32>,
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
    board_view: Arc<BoardView>,
    model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
    model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    device: Arc<EngineDevice>,
    graphics: Arc<EngineGraphicsPipeline>,
    selected_graphics: Arc<EngineGraphicsPipeline>,
    instances: Subbuffer<[TileInstance]>,
    descriptor_set: EngineDescriptorSet,
}

impl BoardRenderer {
    fn new(
        board_view: Arc<BoardView>,
        transform: Arc<Uniform<MN, ModelTransform>>,
        camera: Arc<Uniform<VP, Camera>>,
        model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
        model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
        renderer: &Arc<Renderer>,
        subpass: &Subpass,
    ) -> Self {
        let board_vert = board_vert::load(renderer.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let board_frag = board_frag::load(renderer.device().vk_device().clone())
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

        let selected_create_info = GraphicsPipelineCreateInfo {
            color_blend_state: Some(ColorBlendState::with_attachment_states(
                subpass.num_color_attachments(),
                ColorBlendAttachmentState {
                    blend: Some(AttachmentBlend::alpha()),
                    ..Default::default()
                },
            )),
            ..create_info.clone()
        };

        let graphics = EngineGraphicsPipeline::new(
            renderer,
            create_info,
            ViewportType::BottomLeft,
        );

        let selected_graphics = EngineGraphicsPipeline::new(
            renderer,
            selected_create_info,
            ViewportType::BottomLeft,
        );

        let instances = board_view.state.read().tiles.iter()
            .map(|tile_view| {
                let pos = tile_view.tile.pos();
                let color = if (pos.x + pos.y) % 2 == 0 {
                    glm::vec4(0.5, 0.5, 0.5, 1.0)
                } else {
                    glm::vec4(0.8, 0.8, 0.8, 1.0)
                };
                TileInstance {
                    offset: glm::vec3(tile_view.offset.x, 0.0, tile_view.offset.y),
                    color,
                }
            })
            .collect::<Vec<_>>();

        let instances = Buffer::from_iter(
            renderer.device().memory_allocator().clone(),
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

        let descriptor_set = EngineDescriptorSet::builder(renderer.clone())
            .layout(descriptor_layout)
            .descriptor_source(0, camera)
            .descriptor_source(1, transform)
            .build();

        Self {
            board_view,
            model_tile,
            model_piece,
            device: renderer.device().clone(),
            graphics,
            selected_graphics,
            instances,
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for BoardRenderer {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        let graphics = self.graphics.vk_graphics();
        let selected_graphics = self.selected_graphics.vk_graphics();
        let model_tile = self.model_tile.read();
        let model_piece = self.model_piece.read();

        builder
            .bind_pipeline_graphics(graphics.clone())?
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                self.descriptor_set.descriptor_set().map_err(VulkanoError::from_validated)?,
            )?;
        model_tile.draw_instanced(builder, self.instances.clone())?;

        let (piece_instances, selected_instance) = {
            let state = self.board_view.state.read();
            let pieces = state.tiles.iter()
                .filter_map(|tile_view| {
                    if let Some(player_idx) = tile_view.tile.owner {
                        let player_info = state.board.player_info(player_idx).unwrap();
                        let color = player_info.color();
                        Some(TileInstance {
                            offset: glm::vec3(tile_view.offset.x, 0.2, tile_view.offset.y),
                            color: glm::vec4(color.x, color.y, color.z, 1.0),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let selected = if let Some(tile_view) = state.selected_tile() {
                if tile_view.tile.owner.is_none() {
                    let player_idx = state.board.current_player_idx();
                    let player_info = state.board.player_info(player_idx).unwrap();
                    let color = player_info.color();
                    Some(TileInstance {
                        offset: glm::vec3(tile_view.offset.x, 0.2, tile_view.offset.y),
                        color: glm::vec4(color.x, color.y, color.z, 0.75),
                    })
                } else {
                    None
                }
            } else {
                None
            };

            (pieces, selected)
        };

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
            ).map_err(VulkanoError::from_validated)?;

            model_piece.draw_instanced(builder, piece_instance_buffer)?;
        }

        if selected_instance.is_some() {
            let selected_instance_buffer = Buffer::from_iter(
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
                selected_instance,
            ).map_err(VulkanoError::from_validated)?;

            builder.bind_pipeline_graphics(selected_graphics.clone())?;
            model_piece.draw_instanced(builder, selected_instance_buffer)?;
        }

        Ok(())
    }
}

struct BoardTextRendererLayout {
    font_sheet: Arc<Resource<FontSheet>>,
    layout: Mutex<TextLayout>,
}

impl BoardTextRendererLayout {
    fn create_layout(
        target: &Arc<dyn RenderTarget>,
        font_sizer: &Arc<dyn FontSizer>,
    ) -> TextLayout {
        let screen_size = target.extent().cast::<f32>();

        TextLayout::for_render_target(
            target,
            font_sizer.clone(),
            TextLayoutCreateInfo {
                alignment: LayoutAlignment {
                    horizontal: LayoutHorizontalAlignment::Center,
                    vertical: LayoutVerticalAlignment::Center,
                },
                size: FontSize::Pt(32.0),
                canvas_offset: glm::vec2(0.0, 0.3 * screen_size.y),
                canvas_size: Some(glm::vec2(screen_size.x, 0.3 * screen_size.y)),
                ..Default::default()
            }
        )
    }

    fn new(renderer: &Arc<Renderer>, font_sheet: Arc<Resource<FontSheet>>) -> Arc<Self> {
        let layout = Self::create_layout(
            renderer.target(),
            font_sheet.read().sizer(),
        );
        let render_layout = Arc::new(Self {
            font_sheet,
            layout: Mutex::new(layout),
        });
        renderer.event_bus().register(render_layout.clone()).unwrap();
        render_layout
    }
}

impl EventHandler<RendererStaleEvent> for BoardTextRendererLayout {
    fn handle_event(&self, event: &mut Event<RendererStaleEvent>) {
        let new_layout = Self::create_layout(
            event.target(),
            self.font_sheet.read().sizer(),
        );
        *self.layout.lock() = new_layout;
    }
}

pub struct BoardTextRenderer {
    board_view: Arc<BoardView>,
    #[allow(unused)]
    font: Arc<Resource<dyn Font>>,
    #[allow(unused)]
    font_sheet: Arc<Resource<FontSheet>>,
    font_compositor: FontCompositor,
    layout: Arc<BoardTextRendererLayout>,
}

impl BoardTextRenderer {
    fn new(
        renderer: &Arc<Renderer>,
        subpass: &Subpass,
        font: Arc<Resource<dyn Font>>,
        board_view: Arc<BoardView>,
    ) -> Self {
        let font_sheet = FontSheet::from_font(
            &font,
            renderer.clone(),
            64.0,
            Arc::new(UnicodeFontSheetMap::basic_latin()),
        ).unwrap();

        let font_compositor = FontCompositor::new(
            FontSheetRenderer::new(
                renderer,
                subpass,
                font_sheet.clone(),
            )
        );

        let layout = BoardTextRendererLayout::new(renderer, font_sheet.clone());

        Self {
            board_view,
            font,
            font_sheet,
            font_compositor,
            layout,
        }
    }
}

impl EngineRenderHandler for BoardTextRenderer {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        let mut pass = self.font_compositor.begin_pass(builder);

        let mut layout = self.layout.layout.lock();
        layout.clear();
        let state = self.board_view.state.read();
        match state.board.game_state() {
            GameState::Draw => {
                layout
                    .color(glm::vec3(1.0, 1.0, 1.0))
                    .text("Game ends in a Draw");
                pass.layout(&layout);
            },
            GameState::PlayerWon(player_idx) => {
                let player_info = state.board.player_info(player_idx).unwrap();
                layout
                    .color(player_info.color())
                    .text(&player_info.name())
                    .color(glm::vec3(1.0, 1.0, 1.0))
                    .text(" Wins!");
                pass.layout(&layout);
            },
            _ => {}
        }

        pass.end_pass()?;

        Ok(())
    }
}