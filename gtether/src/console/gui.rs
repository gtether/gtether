use parking_lot::Mutex;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::{event, Level};
use vulkano::buffer::{BufferContents, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;
use vulkano::Validated;
use winit::event::ElementState;
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::console::log::ConsoleLogRecord;
use crate::console::Console;
use crate::event::{Event, EventHandler};
use crate::gui::input::{InputDelegateEvent, InputDelegateLock};
use crate::gui::window::winit::WindowHandle;
use crate::render::font::compositor::FontCompositor;
use crate::render::font::layout::{LayoutAlignment, LayoutHorizontalAlignment, LayoutVerticalAlignment, TextLayout, TextLayoutCreateInfo};
use crate::render::font::sheet::{FontSheet, FontSheetRenderer, UnicodeFontSheetMap};
use crate::render::font::size::{FontSize, FontSizer};
use crate::render::font::Font;
use crate::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use crate::render::render_pass::EngineRenderHandler;
use crate::render::uniform::Uniform;
use crate::render::{FlatVertex, RenderTarget, Renderer, RendererStaleEvent, VulkanoError};
use crate::resource::{Resource, ResourceLoadError};
use crate::NonExhaustive;
use crate::render::descriptor_set::EngineDescriptorSet;

/// General alignment configuration for a [ConsoleGui] section.
#[derive(Debug, Clone)]
pub enum ConsoleGuiAlignment {
    /// Align the section to the top of the screen and/or gui.
    Top,
    /// Align the section to the bottom of the screen and/or gui.
    Bottom,
}

/// Configuration of how to structure a [ConsoleGui].
#[derive(Debug, Clone)]
pub struct ConsoleGuiLayout {
    /// How much of the vertical screen space the [ConsoleGui] covers.
    ///
    /// Default is 0.4, or 40%.
    pub size: f32,

    /// Overall alignment of the [ConsoleGui] vertically on the screen.
    ///
    /// Default is anchored to the top of the screen.
    pub alignment: ConsoleGuiAlignment,

    /// Alignment of the prompt below or above the console log.
    ///
    /// Default is below the console log.
    pub prompt_alignment: ConsoleGuiAlignment,

    /// [FontSize] to use for the [ConsoleGui].
    ///
    /// Default is [FontSize::default()].
    pub font_size: FontSize,

    pub _ne: NonExhaustive,
}

impl Default for ConsoleGuiLayout {
    fn default() -> Self {
        Self {
            size: 0.4,
            alignment: ConsoleGuiAlignment::Top,
            prompt_alignment: ConsoleGuiAlignment::Bottom,
            font_size: FontSize::default(),
            _ne: NonExhaustive(()),
        }
    }
}

impl ConsoleGuiLayout {
    /// Calculate the (x,y) min and max bounds for the screen space the [ConsoleGui] takes up.
    ///
    /// Bounds are between 0.0 and 1.0, inclusive.
    pub fn bounds(&self) -> (glm::TVec2<f32>, glm::TVec2<f32>) {
        match self.alignment {
            ConsoleGuiAlignment::Top => (
                glm::vec2(0.0, 0.0),
                glm::vec2(1.0, self.size),
            ),
            ConsoleGuiAlignment::Bottom => (
                glm::vec2(0.0, 1.0 - self.size),
                glm::vec2(1.0, 1.0)
            ),
        }
    }

    /// Calculate the Vulkan (x,y) min and max screen bounds for the [ConsoleGui].
    ///
    /// Whereas [ConsoleGuiLayout::bounds()] is between 0.0 and 1.0, this method returns bounds
    /// between -1.0 and 1.0, which is suitable for the Vulkan screen coordinate system.
    pub fn screen_bounds(&self) -> (glm::TVec2<f32>, glm::TVec2<f32>) {
        let (min, max) = self.bounds();
        (glm::vec2(
            min.x * 2.0 - 1.0,
            min.y * 2.0 - 1.0,
        ),
        glm::vec2(
            max.x * 2.0 - 1.0,
            max.y * 2.0 - 1.0,
        ))
    }
}

/// [ConsoleGui] background configuration for a solid background color.
#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct BackgroundSolid {
    /// RGB color to use.
    ///
    /// Default is (0.1, 0.1, 0.1), which is a very dark grey.
    pub color: glm::TVec3<f32>,

    /// Alpha transparency to use.
    ///
    /// Default is 0.8, or 80%.
    pub alpha: f32,
}

impl Default for BackgroundSolid {
    fn default() -> Self {
        Self {
            color: glm::vec3(0.1, 0.1, 0.1),
            alpha: 0.8,
        }
    }
}

/// [ConsoleGui] background configuration.
#[derive(Debug, Clone)]
pub enum ConsoleGuiBackground {
    /// A solid background color with transparency.
    Solid(BackgroundSolid),
    // TODO: Add an image background option
}

impl Default for ConsoleGuiBackground {
    fn default() -> Self {
        ConsoleGuiBackground::Solid(BackgroundSolid::default())
    }
}

/// GUI representation of a [Console].
///
/// Wrapper around a [Console] used to both display and interact with said [Console]. To display the
/// [ConsoleGui], an [EngineRenderHandler] is created alongside new [ConsoleGui]s, and may be used
/// like any other [EngineRenderHandler]. When a [ConsoleGui] is created, a background thread is
/// automatically started to handle input events from the window that the [ConsoleGui] is attached
/// to.
///
/// A [ConsoleGui] is attached to a specific [Console] + [Window](WindowHandle) combination, but a
/// [Console] may have multiple [ConsoleGui]s.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use gtether::console::Console;
/// use gtether::console::gui::ConsoleGui;
/// # use gtether::gui::window::winit::WindowHandle;
/// # use gtether::render::font::Font;
/// # use gtether::render::render_pass::EngineRenderSubpassBuilder;
/// # use gtether::resource::Resource;
/// #
/// # let console: Arc<Console> = return;
/// # let window_handle: WindowHandle = return;
/// # let font: Arc<Resource<dyn Font>> = return;
/// # let render_subpass_builder: EngineRenderSubpassBuilder = return;
///
/// let console_gui = ConsoleGui::builder(console.clone())
///     .window(&window_handle)
///     .font(font.clone())
///     .build().unwrap();
///
/// // Later, when building a render pass
/// render_subpass_builder
///     .handler(console_gui.bootstrap_renderer());
/// ```
///
/// Creating GUIs for multiple windows from one console
/// ```
/// # use std::sync::Arc;
/// # use gtether::console::Console;
/// use gtether::console::gui::ConsoleGui;
/// # use gtether::gui::window::winit::WindowHandle;
/// # use gtether::render::font::Font;
/// # use gtether::resource::Resource;
/// #
/// # let console: Arc<Console> = return;
/// # let window_1: WindowHandle = return;
/// # let window_2: WindowHandle = return;
/// # let font: Arc<Resource<dyn Font>> = return;
///
/// let builder = ConsoleGui::builder(console.clone())
///     .font(font.clone());
///
/// let console_gui_1 = builder.window(&window_1).build().unwrap();
/// let console_gui_2 = builder.window(&window_2).build().unwrap();
/// ```
pub struct ConsoleGui {
    console: Arc<Console>,
    visible: AtomicBool,
    layout: ConsoleGuiLayout,
    background: ConsoleGuiBackground,
    // Font is required here to keep the resource alive even if it's not used elsewhere
    #[allow(unused)]
    font: Arc<Resource<dyn Font>>,
    font_sheet: Arc<Resource<FontSheet>>,
    text_log: Mutex<TextLayout>,
    text_prompt: Mutex<TextLayout>,
}

impl ConsoleGui {
    /// Create a [ConsoleGuiBuilder].
    ///
    /// This is the recommended method of creating a [ConsoleGui].
    #[inline]
    pub fn builder(console: Arc<Console>) -> ConsoleGuiBuilder { ConsoleGuiBuilder::new(console) }

    fn create_text_layouts(
        target: &Arc<dyn RenderTarget>,
        layout: &ConsoleGuiLayout,
        font_sizer: &Arc<dyn FontSizer>,
    ) -> (TextLayout, TextLayout) {
        let screen_size = target.extent().cast::<f32>();

        let (bounds_min, bounds_max) = layout.bounds();
        let scaled_font_size = layout.font_size.scale(target.scale_factor() as f32);
        let prompt_height = font_sizer.v_advance(scaled_font_size);
        let prompt_padding = font_sizer.line_gap(scaled_font_size) * 2.0;
        let log_height = ((bounds_max.y - bounds_min.y) * screen_size.y) - (prompt_height + prompt_padding);
        let start_offset = glm::vec2(
            bounds_min.x * screen_size.x,
            bounds_min.y * screen_size.y,
        );

        let log_offset = match &layout.prompt_alignment {
            ConsoleGuiAlignment::Top => glm::vec2(
                start_offset.x,
                start_offset.y + prompt_height + prompt_padding,
            ),
            ConsoleGuiAlignment::Bottom => start_offset.clone(),
        };
        let log_size = glm::vec2(screen_size.x, log_height);

        let prompt_offset = match &layout.prompt_alignment {
            ConsoleGuiAlignment::Top => start_offset.clone(),
            ConsoleGuiAlignment::Bottom => glm::vec2(
                start_offset.x,
                start_offset.y + log_height + prompt_padding,
            )
        };
        let prompt_size = glm::vec2(screen_size.x, prompt_height);

        let text_log = TextLayout::for_render_target(
            target,
            font_sizer.clone(),
            TextLayoutCreateInfo {
                alignment: LayoutAlignment {
                    horizontal: LayoutHorizontalAlignment::Left,
                    vertical: match &layout.prompt_alignment {
                        ConsoleGuiAlignment::Top => LayoutVerticalAlignment::Top,
                        ConsoleGuiAlignment::Bottom => LayoutVerticalAlignment::Bottom,
                    },
                },
                size: layout.font_size,
                canvas_offset: log_offset,
                canvas_size: Some(log_size),
                word_wrap: true,
                ..Default::default()
            }
        );

        let text_prompt = TextLayout::for_render_target(
            target,
            font_sizer.clone(),
            TextLayoutCreateInfo {
                size: layout.font_size,
                canvas_offset: prompt_offset,
                canvas_size: Some(prompt_size),
                ..Default::default()
            }
        );

        (text_log, text_prompt)
    }

    fn new(
        console: Arc<Console>,
        window_handle: &WindowHandle,
        layout: ConsoleGuiLayout,
        background: ConsoleGuiBackground,
        font: Arc<Resource<dyn Font>>,
        font_sheet: Arc<Resource<FontSheet>>,
    ) -> Arc<Self> {
        let delegate = window_handle.input_state().create_delegate();

        let (text_log, text_prompt) = Self::create_text_layouts(
            window_handle.renderer().target(),
            &layout,
            font_sheet.read().sizer(),
        );

        let orig_gui = Arc::new(Self {
            console,
            visible: AtomicBool::new(false),
            layout,
            background,
            font,
            font_sheet,
            text_log: Mutex::new(text_log),
            text_prompt: Mutex::new(text_prompt),
        });
        window_handle.renderer().event_bus().register(Arc::downgrade(&orig_gui)).unwrap();

        let gui = orig_gui.clone();
        thread::spawn(move || {
            let mut input_lock: Option<InputDelegateLock> = None;
            delegate.start(|delegate, event| {
                let consume = match &event {
                    InputDelegateEvent::Key(event) => {
                        if event.state == ElementState::Pressed {
                            if event.physical_key == PhysicalKey::Code(KeyCode::Backquote) {
                                let visible = !gui.visible.fetch_update(
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                    |x| Some(!x),
                                ).unwrap();

                                event!(Level::DEBUG, visible, "ConsoleGui toggled");

                                if visible {
                                    input_lock = Some(delegate.lock(100));
                                } else {
                                    input_lock = None;
                                }
                                true
                            } else if event.physical_key == PhysicalKey::Code(KeyCode::Enter) {
                                if gui.visible.load(Ordering::Relaxed) {
                                    let cmd = {
                                        let mut input = gui.text_prompt.lock();
                                        let cmd = input.to_string();
                                        input.clear();
                                        cmd
                                    };
                                    let _ = gui.console.handle_command(cmd);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    },
                    _ => false,
                };
                if !consume && gui.visible.load(Ordering::Relaxed) {
                    gui.text_prompt.lock().handle_input_event(&event);
                }
            });
        });

        orig_gui
    }

    /// Generate a callback that creates a [ConsoleRenderer] for this [ConsoleGui].
    ///
    /// Intended for use while [building render passes][rpb]:
    /// ```
    /// # use std::sync::Arc;
    /// # use gtether::console::gui::ConsoleGui;
    /// # use gtether::render::render_pass::EngineRenderSubpassBuilder;
    /// #
    /// # let render_subpass_builder: EngineRenderSubpassBuilder = return;
    /// # let console_gui: Arc<ConsoleGui> = return;
    /// #
    /// render_subpass_builder
    ///     .handler(console_gui.bootstrap_renderer());
    /// ```
    ///
    /// [rpb]: crate::render::render_pass::EngineRenderPassBuilder
    pub fn bootstrap_renderer(self: &Arc<Self>)
            -> impl FnOnce(&Arc<Renderer>, &Subpass) -> ConsoleRenderer {
        let self_clone = self.clone();
        |renderer, subpass| {
            ConsoleRenderer::new(renderer, subpass, self_clone)
        }
    }
}

impl EventHandler<RendererStaleEvent> for ConsoleGui {
    fn handle_event(&self, event: &mut Event<RendererStaleEvent>) {
        let target = event.target();

        let (mut new_text_log, mut new_text_prompt) = Self::create_text_layouts(
            target,
            &self.layout,
            self.font_sheet.read().sizer(),
        );

        let mut text_log = self.text_log.lock();
        let mut text_prompt = self.text_prompt.lock();

        new_text_log.text_layout(&text_log);
        new_text_prompt.text_layout(&text_prompt);

        *text_log = new_text_log;
        *text_prompt = new_text_prompt;
    }
}

mod background_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        src: r"
            #version 460

            layout(location = 0) in vec2 position;

            void main() {
                gl_Position = vec4(position, 0.0, 1.0);
            }
        ",
    }
}

mod background_solid_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 460

            layout(location = 0) out vec4 f_color;

            layout(set = 0, binding = 0) uniform Background_Solid_Color_Data {
                vec3 color;
                float alpha;
            } color;

            void main() {
                f_color = vec4(color.color, color.alpha);
            }
        ",
    }
}

struct ConsoleBackgroundSolidRenderer {
    graphics: Arc<EngineGraphicsPipeline>,
    buffer: Subbuffer<[FlatVertex]>,
    descriptor_set: EngineDescriptorSet,
}

impl ConsoleBackgroundSolidRenderer {
    fn new(renderer: &Arc<Renderer>, subpass: &Subpass, gui: &Arc<ConsoleGui>, bg: BackgroundSolid) -> Self {
        let (min, max) = gui.layout.screen_bounds();
        let buffer = FlatVertex::buffer(
            renderer.device().memory_allocator().clone(),
            min,
            max,
        );

        let background_vert = background_vert::load(renderer.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let background_frag = background_solid_frag::load(renderer.device().vk_device().clone())
            .expect("Failed to create fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some(FlatVertex::per_vertex()
            .definition(&background_vert.info().input_interface)
            .unwrap());

        let stages = [
            PipelineShaderStageCreateInfo::new(background_vert),
            PipelineShaderStageCreateInfo::new(background_frag),
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
                    blend: Some(AttachmentBlend::alpha()),
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

        let color = Arc::new(Uniform::new(
            renderer.device().clone(),
            renderer.frame_manager(),
            bg,
        ).unwrap());

        let descriptor_set = EngineDescriptorSet::builder(renderer.clone())
            .layout(descriptor_layout)
            .descriptor_source(0, color)
            .build();

        Self {
            graphics,
            buffer,
            descriptor_set,
        }
    }

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
            .bind_vertex_buffers(0, self.buffer.clone())?
            .draw(
                self.buffer.len() as u32,
                1,
                0,
                0,
            )?;
        Ok(())
    }
}

enum ConsoleBackgroundRenderer {
    Solid(ConsoleBackgroundSolidRenderer),
}

impl ConsoleBackgroundRenderer {
    fn new(renderer: &Arc<Renderer>, subpass: &Subpass, gui: &Arc<ConsoleGui>) -> Self {
        match &gui.background {
            ConsoleGuiBackground::Solid(bg) => ConsoleBackgroundRenderer::Solid(
                ConsoleBackgroundSolidRenderer::new(renderer, subpass, gui, bg.clone())
            ),
        }
    }

    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        match self {
            ConsoleBackgroundRenderer::Solid(renderer)
                => renderer.build_commands(builder),
        }
    }
}

/// [RendererHandler][erh] for a [ConsoleGui].
///
/// See [ConsoleGui] documentation for more.
///
/// [erh]: EngineRenderHandler
pub struct ConsoleRenderer {
    gui: Arc<ConsoleGui>,
    background: ConsoleBackgroundRenderer,
    font_compositor: FontCompositor,
}

impl ConsoleRenderer {
    /// Create a new [ConsoleRenderer].
    ///
    /// This method exists to use [ConsoleRenderer] with custom render pass implementations. If you
    /// are using the engine-provided [render pass][rp], it is recommended to use the
    /// [bootstrap][bs] method with the relevant builder pattern instead.
    ///
    /// [rp]: crate::render::render_pass::EngineRenderPassBuilder
    /// [bs]: ConsoleGui::bootstrap_renderer
    pub fn new(renderer: &Arc<Renderer>, subpass: &Subpass, gui: Arc<ConsoleGui>) -> Self {
        let background = ConsoleBackgroundRenderer::new(renderer, subpass, &gui);

        let font_compositor = FontCompositor::new(
            FontSheetRenderer::new(
                renderer,
                subpass,
                gui.font_sheet.clone(),
            ),
        );

        Self {
            gui,
            background,
            font_compositor,
        }
    }

    fn add_record_to_layout(record: &ConsoleLogRecord, layout: &mut TextLayout) {
        let gray = glm::vec3(0.5, 0.5, 0.5);
        let level_color = match record.level {
            Level::TRACE => glm::vec3(0.8, 0.5, 0.75),
            Level::DEBUG => glm::vec3(0.2, 0.7, 1.0),
            Level::INFO => glm::vec3(0.4, 0.8, 0.4),
            Level::WARN => glm::vec3(0.85, 0.6, 0.2),
            Level::ERROR => glm::vec3(0.9, 0.2, 0.2),
        };

        layout
            .color(gray).text(&record.date_time.format("%H:%M:%S ").to_string())
            .color(level_color).text(&record.level.to_string())
            .color(glm::vec3(1.0, 1.0, 1.0)).text(" ").text(&record.message);
    }
}

impl EngineRenderHandler for ConsoleRenderer {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        if !self.gui.visible.load(Ordering::Relaxed) {
            // Nothing to draw
            return Ok(());
        }

        self.background.build_commands(builder)?;

        let mut log_layout = self.gui.text_log.lock();
        let prompt_layout = self.gui.text_prompt.lock();

        log_layout.clear();
        match &self.gui.layout.prompt_alignment {
            ConsoleGuiAlignment::Top => {
                for record in self.gui.console.log().iter().rev() {
                    Self::add_record_to_layout(&record, &mut log_layout);
                    if log_layout.height() < log_layout.info().canvas().size.y {
                        log_layout.newline();
                    } else {
                        break;
                    }
                }
            },
            ConsoleGuiAlignment::Bottom => {
                for record in self.gui.console.log().iter().rev() {
                    Self::add_record_to_layout(&record, &mut log_layout);
                    if log_layout.height() < log_layout.info().canvas().size.y {
                        log_layout.move_cursor_to(glm::vec2(0, 0));
                        log_layout.newline();
                        log_layout.move_cursor_to(glm::vec2(0, 0));
                    } else {
                        break;
                    }
                }
            },
        }

        let mut pass = self.font_compositor.begin_pass(builder);
        pass.layout(&log_layout);
        pass.layout(&prompt_layout);
        pass.end_pass()?;

        Ok(())
    }
}

/// Possible errors when building a [ConsoleGui] using [ConsoleGuiBuilder].
#[derive(Debug)]
pub enum ConsoleGuiBuildError {
    /// The [window](ConsoleGuiBuilder::window) parameter was not set.
    MissingWindowParam,
    /// The [font](ConsoleGuiBuilder::font) parameter was not set.
    MissingFontParam,
    /// The [layout](ConsoleGuiBuilder::layout) size was out of bounds (must be within (0.0..1.0]).
    LayoutSizeOutOfBounds(f32),
    /// There was an issue when creating a [FontSheet] from the provided [Font].
    ResourceLoadError(ResourceLoadError),
}

impl Display for ConsoleGuiBuildError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsoleGuiBuildError::MissingWindowParam
                => f.write_str(".window() must be set"),
            ConsoleGuiBuildError::MissingFontParam
                => f.write_str(".font() must be set"),
            ConsoleGuiBuildError::LayoutSizeOutOfBounds(size)
                => write!(f, "Size ({size}) must be within (0.0..1.0]"),
            ConsoleGuiBuildError::ResourceLoadError(err)
                => err.fmt(f),
        }
    }
}

impl Error for ConsoleGuiBuildError {}

impl ConsoleGuiBuildError {
    fn check_layout(layout: ConsoleGuiLayout) -> Result<ConsoleGuiLayout, ConsoleGuiBuildError> {
        if layout.size < 0.0 || layout.size > 1.0 {
            Err(ConsoleGuiBuildError::LayoutSizeOutOfBounds(layout.size))
        } else {
            Ok(layout)
        }
    }
}

/// Builder pattern for [ConsoleGui].
///
/// See [ConsoleGui] documentation for examples of usage.
pub struct ConsoleGuiBuilder {
    console: Arc<Console>,
    window_handle: Option<WindowHandle>,
    layout: Option<ConsoleGuiLayout>,
    background: Option<ConsoleGuiBackground>,
    font: Option<Arc<Resource<dyn Font>>>,
}

impl ConsoleGuiBuilder {
    /// Create a new [ConsoleGuiBuilder].
    ///
    /// It is recommended to use [ConsoleGui::builder()] instead.
    #[inline]
    pub fn new(console: Arc<Console>) -> Self {
        Self {
            console,
            window_handle: None,
            layout: None,
            background: None,
            font: None,
        }
    }

    /// Set the [window][wh] that this [ConsoleGui] is for.
    ///
    /// This is a required parameter, and [Self::build()] will fail without it.
    ///
    /// [wh]: WindowHandle
    pub fn window(&mut self, window_handle: &WindowHandle) -> &mut Self {
        self.window_handle = Some(window_handle.clone());
        self
    }

    /// Set the [layout][lay] that this [ConsoleGui] will use.
    ///
    /// If not set, a default layout will be used.
    ///
    /// [lay]: ConsoleGuiLayout
    pub fn layout(&mut self, layout: ConsoleGuiLayout) -> &mut Self {
        self.layout = Some(layout);
        self
    }

    /// Set the [background][bg] that this [ConsoleGui] will use.
    ///
    /// If not set, a default background will be used.
    ///
    /// [bg]: ConsoleGuiBackground
    pub fn background(&mut self, background: ConsoleGuiBackground) -> &mut Self {
        self.background = Some(background);
        self
    }

    /// Set the [font][ft] that this [ConsoleGui] will use.
    ///
    /// This is a required parameter, and [Self::build()] will fail without it.
    ///
    /// [ft]: Font
    pub fn font(&mut self, font: Arc<Resource<dyn Font>>) -> &mut Self {
        self.font = Some(font);
        self
    }

    /// Build a new [ConsoleGui].
    ///
    /// This is a non-consuming operation, so multiple GUIs can be built from one builder.
    ///
    /// # Errors
    /// Will [error][er] if any required parameters are missing, or if a [FontSheet] cannot be made
    /// from the provided [Font].
    ///
    /// [er]: ConsoleGuiBuildError
    pub fn build(&self) -> Result<Arc<ConsoleGui>, ConsoleGuiBuildError> {
        let console = self.console.clone();
        let window_handle = self.window_handle.as_ref()
            .ok_or(ConsoleGuiBuildError::MissingWindowParam)?;
        let layout = ConsoleGuiBuildError::check_layout(self.layout.clone().unwrap_or_default())?;
        let background = self.background.clone().unwrap_or_default();
        let font = self.font.clone()
            .ok_or(ConsoleGuiBuildError::MissingFontParam)?;

        let font_sheet = FontSheet::from_font(
            &font,
            window_handle.renderer().clone(),
            64.0,
            Arc::new(UnicodeFontSheetMap::basic_latin()),
        ).map_err(ConsoleGuiBuildError::ResourceLoadError)?;

        let console_gui = ConsoleGui::new(
            console,
            window_handle,
            layout,
            background,
            font,
            font_sheet,
        );

        Ok(console_gui)
    }
}