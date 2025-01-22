use std::collections::HashMap;
use gtether::render::attachment::AttachmentMap;
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::model::{Model, ModelVertexNormal};
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::swapchain::Framebuffer;
use gtether::render::uniform::Uniform;
use gtether::render::{Device, RenderTarget, RendererEventData, RendererEventType, RendererHandle};
use gtether::resource::Resource;
use itertools::Itertools;
use parry3d::na::Point3;
use parry3d::query::{Ray, RayCast};
use std::sync::Arc;
use std::thread;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use parry3d::bounding_volume::Aabb;
use tracing::{debug, debug_span, info};
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
use gtether::event::{Event, EventHandler};
use gtether::gui::input::{ElementState, InputDelegate, InputDelegateEvent, MouseButton};
use gtether::render::font::compositor::FontCompositor;
use gtether::render::font::Font;
use gtether::render::font::layout::{LayoutAlignment, LayoutHorizontalAlignment, LayoutVerticalAlignment, TextLayout, TextLayoutCreateInfo};
use gtether::render::font::sheet::{FontSheet, FontSheetRenderer, UnicodeFontSheetMap};
use gtether::render::font::size::{FontSize, FontSizer};
use crate::player::Player;
use crate::render_util::{Camera, ModelTransform, MN, VP};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum GameState {
    InProgress,
    Draw,
    PlayerWon(Player),
}

#[derive(Debug)]
pub struct Tile {
    pos: glm::TVec2<usize>,
    offset: glm::TVec2<f32>,
    aabb: Aabb,
    owner: Option<usize>,
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
            owner: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    #[inline]
    pub fn offset(&self) -> glm::TVec2<i64> {
        match self {
            Self::Up => glm::vec2(0, 1),
            Self::Down => glm::vec2(0, -1),
            Self::Left => glm::vec2(-1, 0),
            Self::Right => glm::vec2(1, 0),
        }
    }

    #[inline]
    pub fn all() -> [Direction; 4] {
        [
            Self::Up,
            Self::Down,
            Self::Left,
            Self::Right,
        ]
    }

    #[inline]
    pub fn iter(self, current_pos: glm::TVec2<usize>, size: glm::TVec2<usize>) -> DirectionIter {
        DirectionIter {
            direction: self,
            current_pos,
            size,
        }
    }
}

pub struct DirectionIter {
    direction: Direction,
    current_pos: glm::TVec2<usize>,
    size: glm::TVec2<usize>,
}

impl Iterator for DirectionIter {
    type Item = glm::TVec2<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_pos.x < self.size.x && self.current_pos.y < self.size.y {
            let new_pos = self.current_pos.cast::<i64>() + self.direction.offset();
            if new_pos.x < 0 || new_pos.y < 0 || new_pos.x >= self.size.x as i64 || new_pos.y >= self.size.y as i64 {
                None
            } else {
                self.current_pos = (self.current_pos.cast::<i64>() + self.direction.offset())
                    .try_cast::<usize>()?;
                Some(self.current_pos)
            }
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct BoardState {
    size: glm::TVec2<usize>,
    tiles: Vec<Tile>,
    selected_pos: Option<glm::TVec2<usize>>,
    players: Vec<Player>,
    current_player_idx: usize,
    turn_no: usize,
    valid_moves_cache: Vec<glm::TVec2<usize>>,
    game_state: GameState,
}

impl BoardState {
    fn new(size: glm::TVec2<usize>, players: Vec<Player>) -> Self {
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

        let mut state = Self {
            size,
            tiles,
            selected_pos: None,
            players,
            current_player_idx: 0,
            turn_no: 1,
            valid_moves_cache: Vec::new(),
            game_state: GameState::InProgress,
        };
        state.update_valid_moves_cache();

        state
    }

    #[inline]
    pub fn tile(&self, pos: glm::TVec2<usize>) -> Option<&Tile> {
        self.tiles.get(pos.x + (pos.y * self.size.x))
    }

    #[inline]
    pub fn tile_mut(&mut self, pos: glm::TVec2<usize>) -> Option<&mut Tile> {
        self.tiles.get_mut(pos.x + (pos.y * self.size.x))
    }

    #[inline]
    pub fn selected_tile(&self) -> Option<&Tile> {
        if let Some(selected_pos) = self.selected_pos {
            self.tile(selected_pos)
        } else {
            None
        }
    }

    #[inline]
    pub fn selected_tile_mut(&mut self) -> Option<&mut Tile> {
        if let Some(selected_pos) = self.selected_pos {
            self.tile_mut(selected_pos)
        } else {
            None
        }
    }

    pub fn valid_selection(&self, pos: glm::TVec2<usize>) -> bool {
        if self.game_state != GameState::InProgress {
            // Game has already ended
            return false;
        }

        if let Some(tile) = self.tile(pos) {
            if tile.owner.is_some() {
                // Obviously can't play on a tile that is already owned
                return false;
            }
        } else {
            // Also can't play on a tile that doesn't exist
            return false;
        }

        if self.turn_no <= 4 {
            // During first 4 turns, only center 4 spaces are valid
            let center = glm::vec2(self.size.x / 2, self.size.y / 2);
            if pos.x != center.x && pos.x != (center.x - 1) {
                return false;
            }
            if pos.y != center.y && pos.y != (center.y - 1) {
                return false;
            }
            true
        } else {
            // After the first 4 turns, every move must capture at least one piece
            Direction::all().into_iter().any(|direction| {
                self.valid_for_flip(pos, direction, self.current_player_idx)
            })
        }
    }

    #[inline]
    pub fn player(&self, player_idx: usize) -> Option<&Player> {
        self.players.get(player_idx)
    }

    #[inline]
    pub fn current_player(&self) -> &Player {
        &self.players[self.current_player_idx]
    }

    fn update_valid_moves_cache(&mut self) {
        self.valid_moves_cache.clear();
        for x in 0..self.size.x {
            for y in 0..self.size.y {
                let pos = glm::vec2(x, y);
                if self.valid_selection(pos) {
                    self.valid_moves_cache.push(pos);
                }
            }
        }
        debug!(valid_move_count = ?self.valid_moves_cache.len(), "Updated valid moves cache");
    }

    pub fn end_turn(&mut self) {
        self.current_player_idx += 1;
        if self.current_player_idx >= self.players.len() {
            self.current_player_idx = 0;
        }
        self.turn_no += 1;
        info!(player = ?self.current_player(), turn_no = ?self.turn_no, "Next turn");
        self.update_valid_moves_cache();
        if self.valid_moves_cache.is_empty() {
            self.end_game();
        }
    }

    pub fn end_game(&mut self) {
        let mut player_counts = HashMap::<usize, usize>::new();
        for x in 0..self.size.x {
            for y in 0..self.size.y {
                if let Some(tile) = self.tile(glm::vec2(x, y)) {
                    if let Some(owner) = tile.owner {
                        *player_counts.entry(owner).or_default() += 1;
                    }
                }
            }
        }
        let mut high_scores = player_counts.into_iter()
            .max_set_by(|(_, count_a), (_, count_b)| {
                count_a.cmp(count_b)
            });
        if high_scores.len() == 1 {
            let (player_idx, _) = high_scores.pop().unwrap();
            let player = self.player(player_idx).unwrap().clone();
            self.game_state = GameState::PlayerWon(player);
        } else {
            self.game_state = GameState::Draw;
        }
        info!(state = ?self.game_state, "Game ended");
    }

    #[inline]
    pub fn game_state(&self) -> GameState {
        self.game_state.clone()
    }

    fn valid_for_flip(&self, start_pos: glm::TVec2<usize>, direction: Direction, player_idx: usize) -> bool {
        let mut pieces_flipped: usize = 0;
        for current_pos in direction.iter(start_pos, self.size) {
            if let Some(tile) = self.tile(current_pos) {
                if let Some(owner) = tile.owner {
                    if owner == player_idx {
                        // Found same owner, all tiles in-between are valid for flipping, if there
                        // are any
                        return pieces_flipped > 0;
                    } else {
                        // Else it's some other owned tile, continue searching
                        pieces_flipped += 1;
                    }
                } else {
                    // Found unowned space, not valid for a flip
                    return false;
                }
            } else {
                // Hit edge of board, not valid for a flip
                return false;
            }
        }
        // Hit edge of board, not valid for a flip
        false
    }

    fn flip(&mut self, start_pos: glm::TVec2<usize>, direction: Direction, player_idx: usize) {
        for current_pos in direction.iter(start_pos, self.size) {
            if let Some(tile) = self.tile_mut(current_pos) {
                if let Some(owner) = tile.owner {
                    if owner == player_idx {
                        // Found same owner, reached end of flipping
                        return;
                    } else {
                        tile.owner = Some(player_idx);
                    }
                } else {
                    // No owner, stop flipping
                    return;
                }
            } else {
                // Hit edge of board, stop flipping
                return;
            }
        }
    }

    pub fn set(&mut self, pos: glm::TVec2<usize>, player_idx: usize) -> bool {
        let _player_span = debug_span!("set_tile", center_pos = ?pos, ?player_idx);

        if player_idx >= self.players.len() {
            return false;
        }

        if let Some(tile) = self.tile_mut(pos) {
            tile.owner = Some(player_idx);
        } else {
            return false;
        }

        for direction in Direction::all() {
            let _direction_span = debug_span!("direction", ?direction);
            if self.valid_for_flip(pos, direction, player_idx) {
                info!("Flipping");
                self.flip(pos, direction, player_idx);
            } else {
                debug!("Flip is NOT valid");
            }
        }

        true
    }

    #[inline]
    pub fn place(&mut self, pos: glm::TVec2<usize>) -> bool {
        if self.valid_selection(pos) {
            self.set(pos, self.current_player_idx)
        } else {
            false
        }
    }

    pub fn reset(&mut self) {
        for tile in self.tiles.iter_mut() {
            tile.owner = None;
        }
        self.selected_pos = None;
        self.current_player_idx = 0;
        self.turn_no = 1;
        self.game_state = GameState::InProgress;
        self.update_valid_moves_cache();
    }
}

#[derive(Debug)]
pub struct Board {
    transform: Arc<Uniform<MN, ModelTransform>>,
    camera: Arc<Uniform<VP, Camera>>,
    state: RwLock<BoardState>,
}

impl Board {
    pub fn new(
        input: InputDelegate,
        transform: Arc<Uniform<MN, ModelTransform>>,
        camera: Arc<Uniform<VP, Camera>>,
        players: Vec<Player>,
        size: glm::TVec2<usize>,
    ) -> Arc<Self> {
        let board = Arc::new(Self {
            transform,
            camera,
            state: RwLock::new(BoardState::new(size, players)),
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

                    let mut state = input_board.state.write();
                    let tile = state.tiles.iter().find(|tile| {
                        tile.aabb.intersects_ray(&transform.transform, &ray, f32::MAX)
                    }).map(|tile| tile.pos);

                    state.selected_pos = None;
                    if let Some(pos) = tile {
                        if state.valid_selection(pos) {
                            state.selected_pos = Some(pos);
                        }
                    }
                },
                InputDelegateEvent::MouseButton(event) => {
                    if event.button == MouseButton::Left && event.state == ElementState::Pressed {
                        let mut state = input_board.state.write();
                        if let Some(selected_pos) = state.selected_pos {
                            if state.place(selected_pos) {
                                state.end_turn();
                            }
                        }
                    }
                }
                _ => {}
            }
        }));

        board
    }

    #[inline]
    pub fn state(&self) -> RwLockReadGuard<BoardState> {
        self.state.read()
    }

    #[inline]
    pub fn reset(&self) {
        self.state.write().reset();
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

    pub fn bootstrap_text_renderer(
        self: &Arc<Self>,
        font: Arc<Resource<dyn Font>>,
    ) -> impl FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>) -> BoardTextRenderer {
        let self_clone = self.clone();
        move |renderer, subpass, _| {
            BoardTextRenderer::new(
                renderer,
                subpass,
                font,
                self_clone,
            )
        }
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
    board: Arc<Board>,
    model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
    model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    device: Arc<Device>,
    graphics: Arc<EngineGraphicsPipeline>,
    selected_graphics: Arc<EngineGraphicsPipeline>,
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

        let instances = board.state.read().tiles.iter()
            .map(|tile| {
                let color = if (tile.pos.x + tile.pos.y) % 2 == 0 {
                    glm::vec4(0.5, 0.5, 0.5, 1.0)
                } else {
                    glm::vec4(0.8, 0.8, 0.8, 1.0)
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
            selected_graphics,
            instances,
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for BoardRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let graphics = self.graphics.vk_graphics();
        let selected_graphics = self.selected_graphics.vk_graphics();
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

        let (piece_instances, selected_instance) = {
            let board_state = self.board.state();
            let pieces = board_state.tiles.iter()
                .filter_map(|tile| {
                    if let Some(player_idx) = tile.owner {
                        let player = board_state.player(player_idx).unwrap();
                        let color = player.color();
                        Some(TileInstance {
                            offset: glm::vec3(tile.offset.x, 0.2, tile.offset.y),
                            color: glm::vec4(color.x, color.y, color.z, 1.0),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let selected = if let Some(tile) = board_state.selected_tile() {
                if tile.owner.is_none() {
                    let player = board_state.current_player();
                    let color = player.color();
                    Some(TileInstance {
                        offset: glm::vec3(tile.offset.x, 0.2, tile.offset.y),
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
            ).unwrap();

            model_piece.draw_instanced(builder, piece_instance_buffer).unwrap();
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
            ).unwrap();

            builder.bind_pipeline_graphics(selected_graphics.clone()).unwrap();
            model_piece.draw_instanced(builder, selected_instance_buffer).unwrap();
        }
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

    fn new(renderer: &RendererHandle, font_sheet: Arc<Resource<FontSheet>>) -> Arc<Self> {
        let layout = Self::create_layout(
            renderer.target(),
            font_sheet.read().sizer(),
        );
        let render_layout = Arc::new(Self {
            font_sheet,
            layout: Mutex::new(layout),
        });
        renderer.event_bus().register(RendererEventType::Stale, render_layout.clone());
        render_layout
    }
}

impl EventHandler<RendererEventType, RendererEventData> for BoardTextRendererLayout {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale);
        let new_layout = Self::create_layout(
            event.target(),
            self.font_sheet.read().sizer(),
        );
        *self.layout.lock() = new_layout;
    }
}

pub struct BoardTextRenderer {
    board: Arc<Board>,
    font: Arc<Resource<dyn Font>>,
    font_sheet: Arc<Resource<FontSheet>>,
    font_compositor: FontCompositor,
    layout: Arc<BoardTextRendererLayout>,
}

impl BoardTextRenderer {
    fn new(
        renderer: &RendererHandle,
        subpass: &Subpass,
        font: Arc<Resource<dyn Font>>,
        board: Arc<Board>,
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
            board,
            font,
            font_sheet,
            font_compositor,
            layout,
        }
    }
}

impl EngineRenderHandler for BoardTextRenderer {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        let mut pass = self.font_compositor.begin_pass(builder, frame);

        let mut layout = self.layout.layout.lock();
        layout.clear();
        match self.board.state().game_state() {
            GameState::Draw => {
                layout
                    .color(glm::vec3(1.0, 1.0, 1.0))
                    .text("Game ends in a Draw");
                pass.layout(&layout);
            },
            GameState::PlayerWon(player) => {
                layout
                    .color(player.color())
                    .text(player.name())
                    .color(glm::vec3(1.0, 1.0, 1.0))
                    .text(" Wins!");
                pass.layout(&layout);
            },
            _ => {}
        }

        pass.end_pass();
    }
}