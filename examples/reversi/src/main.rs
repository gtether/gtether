#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

extern crate nalgebra_glm as glm;

use async_trait::async_trait;
use educe::Educe;
use gtether::client::gui::input::InputDelegate;
use gtether::client::gui::window::{CreateWindowInfo, WindowAttributes, WindowHandle};
use gtether::client::gui::ClientGui;
use gtether::client::Client;
use gtether::console::command::{Command, CommandError, CommandRegistry, ParamCountCheck};
use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::console::Console;
use gtether::event::Event;
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::model::obj::ModelObjLoader;
use gtether::render::model::ModelVertexNormal;
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::uniform::Uniform;
use gtether::render::{RendererEventData, RendererEventType};
use gtether::resource::manager::{LoadPriority, ResourceManager};
use gtether::resource::source::constant::ConstantResourceSource;
use gtether::server::Server;
use gtether::{Application, Engine, EngineBuilder, EngineJoinHandle};
use parking_lot::{Mutex, MutexGuard};
use parry3d::na::Point3;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{debug, info};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use crate::board::Board;
use crate::bot::minimax::MinimaxAlgorithm;
use crate::player::Player;
use crate::render_util::{Camera, DeferredLightingRendererBootstrap, ModelTransform, PointLight};

mod board;
mod bot;
mod render_util;
mod player;

const REVERSI_PORT: u16 = 19502;

#[derive(Debug)]
struct ResetCommand {
    board: Arc<Board>,
}

impl Command for ResetCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(0).check(parameters.len() as u32)?;
        self.board.reset();
        Ok(())
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct ConnectCommand {
    #[educe(Debug(ignore))]
    client: Arc<Engine<ReversiApp, ClientGui>>,
    #[educe(Debug(ignore))]
    server: Arc<ServerContainer>,
}

impl Command for ConnectCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::OneOf(vec![
            ParamCountCheck::Equal(0),
            ParamCountCheck::Equal(1),
        ]).check(parameters.len() as u32)?;

        if parameters.len() == 1 {
            let socket_addr = parameters[0].parse()
                .map_err(|_| CommandError::InvalidParameter(parameters[0].clone()))?;
            self.server.shutdown();
            self.client.side().net().connect_sync(socket_addr)
        } else {
            self.server.start();
            self.client.side().net().connect_sync(SocketAddr::new(
                Ipv4Addr::LOCALHOST.into(),
                REVERSI_PORT,
            ))
        }.map_err(|err| CommandError::CommandFailure(Box::new(err)))?;

        Ok(())
    }
}

struct ReversiApp {
    console: Arc<Console>,
    window: OnceLock<WindowHandle>,
    board: OnceLock<Arc<Board>>,
    input: OnceLock<InputDelegate>,
    server: Arc<ServerContainer>,
}

impl ReversiApp {
    fn new() -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        Self {
            console,
            window: OnceLock::new(),
            board: OnceLock::new(),
            input: OnceLock::new(),
            server: Arc::new(ServerContainer::new()),
        }
    }
}

#[async_trait(?Send)]
impl Application<ClientGui> for ReversiApp {
    async fn init(&self, engine: &Arc<Engine<Self, ClientGui>>) {
        let mut cmd_registry = self.console.registry();

        let window = engine.side().create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Reversi"),
            ..Default::default()
        }).wait_async().await.unwrap();

        let console_font = engine.resources().get_or_load(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
            LoadPriority::Immediate,
        );
        let model_tile = engine.resources().get_or_load(
            "tile.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().device().clone()),
            LoadPriority::Immediate,
        );
        let model_piece = engine.resources().get_or_load(
            "piece.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().device().clone()),
            LoadPriority::Immediate,
        );

        let transform = Arc::new(Uniform::new(
            window.renderer().device().clone(),
            window.renderer().frame_manager(),
            ModelTransform::new(),
        ).unwrap());

        let camera = Arc::new(Uniform::new(
            window.renderer().device().clone(),
            window.renderer().frame_manager(),
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
                Arc::new(Player::human("Player1", glm::vec3(0.05, 0.05, 0.05))),
                Arc::new(Player::bot(
                    "Player2",
                    glm::vec3(0.95, 0.95, 0.95),
                    MinimaxAlgorithm::new(5),
                )),
            ],
            glm::vec2(8, 8),
        );

        cmd_registry.register_command("reset", Box::new(ResetCommand {
            board: board.clone(),
        })).unwrap();

        let deferred_lighting_renderer = DeferredLightingRendererBootstrap::new(
            window.renderer(),
            vec![
                PointLight {
                    position: glm::vec4(-4.0, 10.0, 4.0, 1.0),
                    color: glm::vec3(0.8, 0.8, 0.8),
                }
            ]
        );

        let console_font = console_font.await.unwrap();
        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font.clone())
            .build().unwrap();

        let render_pass = EngineRenderPassBuilder::new(window.renderer().clone())
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
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0]).unwrap()
            .begin_subpass()
                .color_attachment("color")
                .color_attachment("normals")
                .depth_stencil_attachment("depth")
                .handler(board.bootstrap_renderer(
                    model_tile.await.unwrap(),
                    model_piece.await.unwrap(),
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
            .build().unwrap();
        window.renderer().set_render_pass(render_pass);

        cmd_registry.register_command("connect", Box::new(ConnectCommand {
            client: engine.clone(),
            server: self.server.clone()
        })).unwrap();

        self.input.set(window.input_state().create_delegate()).unwrap();
        self.window.set(window).unwrap();
        self.board.set(board).unwrap();
    }

    fn tick(&self, _engine: &Arc<Engine<Self, ClientGui>>, _delta: Duration) {
        /* noop */
    }
}

struct ReversiAppServer {

}

impl ReversiAppServer {
    fn new() -> Self {
        Self {

        }
    }
}

#[async_trait(?Send)]
impl Application<Server> for ReversiAppServer {
    async fn init(&self, _engine: &Arc<Engine<Self, Server>>) {

    }

    fn tick(&self, _engine: &Arc<Engine<Self, Server>>, _delta: Duration) {

    }
}

type EngineServer = Engine<ReversiAppServer, Server>;
type EngineServerJoinHandle = EngineJoinHandle<ReversiAppServer, Server>;

struct ServerContainer {
    inner: Mutex<Option<EngineServerJoinHandle>>,
}

impl ServerContainer {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn shutdown_impl(inner: &mut MutexGuard<Option<EngineServerJoinHandle>>) {
        if let Some(server) = inner.take() {
            info!("Stopping server...");
            server.stop().unwrap();
            info!("Server stopped.");
        } else {
            debug!("Server is already stopped");
        }
    }

    fn shutdown(&self) {
        Self::shutdown_impl(&mut self.inner.lock());
    }

    fn start_impl(inner: &mut MutexGuard<Option<EngineServerJoinHandle>>) {
        info!("Starting server...");
        let side_result = Server::builder()
            .port(REVERSI_PORT)
            .build();
        let side = match side_result {
            Ok(side) => side,
            Err(error) => {
                debug!(?error);
                panic!("{error}");
            },
        };
        let join_handle = EngineBuilder::new()
            .app(ReversiAppServer::new())
            .side(side)
            .spawn();
        **inner = Some(join_handle);
        info!("Server started.");
    }

    fn start(&self) -> Arc<EngineServer> {
        let mut inner = self.inner.lock();
        if inner.is_none() {
            Self::start_impl(&mut inner);
        } else {
            debug!("Server is already running");
        }
        inner.as_ref().unwrap().engine()
    }

    fn restart(&self) -> Arc<EngineServer> {
        let mut inner = self.inner.lock();
        Self::shutdown_impl(&mut inner);
        Self::start_impl(&mut inner);
        inner.as_ref().unwrap().engine()
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
        .app(app)
        .side(Client::builder()
            .application_name("gTether Example - reversi")
            .enable_gui()
            .build().unwrap())
        .resources(resources)
        .build()
        .start();
}
