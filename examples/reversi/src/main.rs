extern crate nalgebra_glm as glm;

use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::console::Console;
use gtether::event::Event;
use gtether::gui::window::{CreateWindowInfo, WindowAttributes, WindowHandle};
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::model::obj::ModelObjLoader;
use gtether::render::model::ModelVertexNormal;
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::uniform::Uniform;
use gtether::render::{RendererEventData, RendererEventType};
use gtether::resource::manager::{LoadPriority, ResourceManager};
use gtether::resource::source::constant::ConstantResourceSource;
use gtether::{Application, Engine, EngineBuilder, EngineMetadata, Registry};
use std::cell::OnceCell;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use crate::board::Board;
use crate::render_util::{DeferredLightingRendererBootstrap, PointLight, VP};

mod board;
mod render_util;

struct ReversiApp {
    console: Arc<Console>,
    window: OnceCell<WindowHandle>,
    board: OnceCell<Arc<Board>>,
}

impl ReversiApp {
    fn new() -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        Self {
            console,
            window: OnceCell::new(),
            board: OnceCell::new(),
        }
    }
}

impl Application for ReversiApp {
    fn init(&self, engine: &Engine<Self>, registry: &mut Registry) {
        let window = registry.window.create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Reversi"),
            ..Default::default()
        });

        let console_font = engine.resources().get_or_load(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
            LoadPriority::Immediate,
        );
        let model_tile = engine.resources().get_or_load(
            "tile.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().target().device().clone()),
            LoadPriority::Immediate,
        );

        let vp = Arc::new(Uniform::new(
            window.renderer().target(),
            VP::default(),
        ).unwrap());
        *vp.write() = VP::look_at(
            &glm::vec3(0.0, 6.0, -3.0),
            &glm::vec3(0.0, 0.0, 0.0),
            &glm::vec3(0.0, 1.0, 0.0),
        );
        {
            let vp = vp.clone();
            window.renderer().event_bus().register(
                RendererEventType::Stale,
                move |event: &mut Event<RendererEventType, RendererEventData>| {
                    vp.write().projection = glm::perspective(
                        event.target().dimensions().aspect_ratio(),
                        glm::half_pi(),
                        0.01, 100.0,
                    );
                }
            );
        }

        let board = Arc::new(Board::new(
            window.renderer().target(),
            model_tile.wait().unwrap(),
            glm::vec2(8, 8),
            vp.clone(),
        ));

        let deferred_lighting_renderer = DeferredLightingRendererBootstrap::new(
            window.renderer().target(),
            vec![
                PointLight {
                    position: glm::vec4(-4.0, 10.0, 4.0, 1.0),
                    color: glm::vec3(0.8, 0.8, 0.8),
                }
            ]
        );

        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font.wait().unwrap())
            .build().unwrap();

        let render_pass = EngineRenderPassBuilder::new(window.renderer())
            .attachment("color", AttachmentDescription {
                format: Format::A2B10G10R10_UNORM_PACK32,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some([0.0, 0.0, 0.0, 1.0]))
            .attachment("normals", AttachmentDescription {
                format: Format::R16G16B16A16_SFLOAT,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some([0.0, 0.0, 0.0, 1.0]))
            .attachment("depth", AttachmentDescription {
                format: Format::D16_UNORM,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some(1.0))
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0])
            .begin_subpass()
                .color_attachment("color")
                .color_attachment("normals")
                .depth_stencil_attachment("depth")
                .handler(board.bootstrap_renderer())
            .end_subpass()
            .begin_subpass()
                .input_attachment("color")
                .input_attachment("normals")
                .color_attachment("final_color")
                .handler(deferred_lighting_renderer.bootstrap())
                .handler(console_gui.bootstrap_renderer())
            .end_subpass()
            .build();
        window.renderer().set_render_pass(render_pass).unwrap();

        self.window.set(window).unwrap();
        self.board.set(board).unwrap();
    }

    fn tick(&self, _engine: &Engine<Self>, _delta: Duration) {
        // noop
    }
}

fn main() {
    let app = ReversiApp::new();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive(LevelFilter::WARN.into())
            .add_directive("reversi=debug".parse().unwrap()))
        .finish()
        .with(ConsoleLogLayer::new(app.console.log()))
        .init();

    let resources = ResourceManager::builder()
        .source(ConstantResourceSource::builder()
            .resource("console_font", include_bytes!("../assets/RobotoMono/RobotoMono-VariableFont_wght.ttf"))
            .resource("tile.obj", include_bytes!("../assets/tile.obj"))
            .build())
        .build();

    EngineBuilder::new()
        .metadata(EngineMetadata {
            application_name: Some("gTether Example - reversi".to_owned()),
            ..Default::default()
        })
        .app(app)
        .resources(resources)
        .build()
        .start();
}
