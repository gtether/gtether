#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

extern crate nalgebra_glm as glm;

use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::console::Console;
use gtether::event::Event;
use gtether::gui::input::InputDelegate;
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
use parry3d::na::Point3;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use crate::board::Board;
use crate::player::Player;
use crate::render_util::{Camera, DeferredLightingRendererBootstrap, ModelTransform, PointLight};

mod board;
mod render_util;
mod player;

struct ReversiApp {
    console: Arc<Console>,
    window: OnceCell<WindowHandle>,
    board: OnceCell<Arc<Board>>,
    input: OnceCell<InputDelegate>,
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
            input: OnceCell::new(),
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
        let model_piece = engine.resources().get_or_load(
            "piece.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().target().device().clone()),
            LoadPriority::Immediate,
        );

        let transform = Arc::new(Uniform::new(
            window.renderer().target(),
            ModelTransform::new(),
        ).unwrap());

        let camera = Arc::new(Uniform::new(
            window.renderer().target(),
            Camera::new(
                window.renderer().target(),
                &Point3::new(0.0, 5.0, -2.0),
                &Point3::new(0.0, 0.0, -0.5),
                &glm::vec3(0.0, 1.0, 0.0),
            ),
        ).unwrap());
        {
            let camera = camera.clone();
            window.renderer().event_bus().register(
                RendererEventType::Stale,
                move |event: &mut Event<RendererEventType, RendererEventData>| {
                    camera.write().update(event.target());
                }
            );
        }

        let board = Board::new(
            window.input_state().create_delegate(),
            transform.clone(),
            camera.clone(),
            vec![
                Player::new("Player1", glm::vec3(0.95, 0.95, 0.95)),
                Player::new("Player2", glm::vec3(0.05, 0.05, 0.05)),
            ],
            glm::vec2(8, 8),
        );

        let deferred_lighting_renderer = DeferredLightingRendererBootstrap::new(
            window.renderer().target(),
            vec![
                PointLight {
                    position: glm::vec4(-4.0, 10.0, 4.0, 1.0),
                    color: glm::vec3(0.8, 0.8, 0.8),
                }
            ]
        );

        let console_font = console_font.wait().unwrap();
        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font.clone())
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
                .handler(board.bootstrap_renderer(
                    model_tile.wait().unwrap(),
                    model_piece.wait().unwrap(),
                ))
            .end_subpass()
            .begin_subpass()
                .input_attachment("color")
                .input_attachment("normals")
                .color_attachment("final_color")
                .handler(deferred_lighting_renderer.bootstrap())
                .handler(board.bootstrap_text_renderer(console_font))
                .handler(console_gui.bootstrap_renderer())
            .end_subpass()
            .build();
        window.renderer().set_render_pass(render_pass).unwrap();

        self.input.set(window.input_state().create_delegate()).unwrap();
        self.window.set(window).unwrap();
        self.board.set(board).unwrap();
    }

    fn tick(&self, _engine: &Engine<Self>, _delta: Duration) {
        /* noop */
    }
}

fn main() {
    let app = ReversiApp::new();

    let subscriber_builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive(LevelFilter::WARN.into())
            .add_directive("reversi=debug".parse().unwrap()));
    #[cfg(not(debug_assertions))]
    let subscriber_builder = subscriber_builder.with_writer(std::io::sink);
    subscriber_builder
        .finish()
        .with(ConsoleLogLayer::new(app.console.log()))
        .init();

    let resources = ResourceManager::builder()
        .source(ConstantResourceSource::builder()
            .resource("console_font", include_bytes!("../assets/RobotoMono/RobotoMono-VariableFont_wght.ttf"))
            .resource("tile.obj", include_bytes!("../assets/tile.obj"))
            .resource("piece.obj", include_bytes!("../assets/piece.obj"))
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
