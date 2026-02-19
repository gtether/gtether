use async_trait::async_trait;
use educe::Educe;
use gtether::app::Application;
use gtether::console::command::{Command, CommandError, CommandRegistry, ParamCountCheck};
use gtether::console::gui::ConsoleGui;
use gtether::console::log::ConsoleLog;
use gtether::console::Console;
use gtether::event::Event;
use gtether::gui::window::winit::{CreateWindowInfo, WindowAttributes, WindowHandle, WinitDriver};
use gtether::net::gns::GnsClientDriver;
use gtether::net::{Networking, NetworkingError};
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::font::Font;
use gtether::render::model::obj::ModelObjLoader;
use gtether::render::model::{Model, ModelVertexNormal};
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::uniform::Uniform;
use gtether::render::RendererStaleEvent;
use gtether::resource::Resource;
use gtether::worker::WorkerPool;
use gtether::Engine;
use parking_lot::{Mutex, RwLock};
use parry3d::na::Point3;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use tracing::info;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use crate::board::view::BoardView;
use crate::render_util::{Camera, DeferredLightingRendererBootstrap, ModelTransform, PointLight, MN, VP};
use crate::server::{PlayerConnect, ReversiServerManager, REVERSI_PORT};

#[derive(Educe)]
#[educe(Debug)]
struct RenderData {
    model_tile: Arc<Resource<Model<ModelVertexNormal>>>,
    model_piece: Arc<Resource<Model<ModelVertexNormal>>>,
    #[educe(Debug(ignore))]
    font: Arc<Resource<dyn Font>>,
    transform: Arc<Uniform<MN, ModelTransform>>,
    camera: Arc<Uniform<VP, Camera>>,
    deferred_lighting: Arc<DeferredLightingRendererBootstrap>,
    #[educe(Debug(ignore))]
    console_gui: Arc<ConsoleGui>,
}

pub struct ReversiClient {
    console: Arc<Console>,
    window: OnceLock<WindowHandle>,
    board_view: Mutex<Option<Arc<BoardView>>>,
    render_data: OnceLock<RenderData>,
    preferred_name: RwLock<String>,
    server: Arc<ReversiServerManager>,
}

impl ReversiClient {
    pub fn new(workers: Arc<WorkerPool<()>>) -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        let server = Arc::new(ReversiServerManager::new(workers.clone()));

        Self {
            console,
            window: OnceLock::new(),
            board_view: Mutex::new(None),
            render_data: OnceLock::new(),
            preferred_name: RwLock::new("Player".to_owned()),
            server,
        }
    }

    #[inline]
    pub fn console(&self) -> &Arc<Console> {
        &self.console
    }

    #[inline]
    pub fn set_preferred_name(&self, name: impl Into<String>) {
        let name = name.into();
        info!(?name, "Set preferred name");
        *self.preferred_name.write() = name;
    }

    fn set_board_view(&self, board_view: Arc<BoardView>) {
        let window = self.window.get()
            .expect("ReversiClient should be initialized");
        let render_data = self.render_data.get()
            .expect("ReversiClient should be initialized");

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
            .handler(board_view.bootstrap_renderer(
                render_data.model_tile.clone(),
                render_data.model_piece.clone(),
            ))
            .end_subpass()
            .begin_subpass()
            .input_attachment("color")
            .input_attachment("normals")
            .color_attachment("final_color")
            .handler(render_data.deferred_lighting.bootstrap())
            .handler(board_view.bootstrap_text_renderer(render_data.font.clone()))
            .handler(render_data.console_gui.bootstrap_renderer())
            .end_subpass()
            .build().unwrap();

        let mut board_view_lock = self.board_view.lock();
        *board_view_lock = Some(board_view);
        window.renderer().set_render_pass(render_pass);
    }

    fn clear_board_view(&self) {
        let window = self.window.get()
            .expect("ReversiClient should be initialized");
        let render_data = self.render_data.get()
            .expect("ReversiClient should be initialized");

        let render_pass = EngineRenderPassBuilder::new(window.renderer().clone())
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0]).unwrap()
            .begin_subpass()
            .color_attachment("final_color")
            .handler(render_data.console_gui.bootstrap_renderer())
            .end_subpass()
            .build().unwrap();
        window.renderer().set_render_pass(render_pass);

        let mut board_view_lock = self.board_view.lock();
        *board_view_lock = None;
    }

    fn connect(
        &self,
        socket_addr: SocketAddr,
        net: &Arc<Networking<GnsClientDriver>>,
    ) -> Result<(), NetworkingError> {
        let window = self.window.get().unwrap();
        let render_data = self.render_data.get().unwrap();

        info!(addr = ?socket_addr, "Connecting to server");
        let _connect_ctx = net.connect_sync(socket_addr)?;

        let msg = PlayerConnect::new(&*self.preferred_name.read(), None);
        let reply = net.send_recv(msg)?.wait();
        let reply_body = reply.into_body();

        let local_players = match reply_body.player_idx() {
            Some(player_idx) => HashSet::from([player_idx]),
            None => HashSet::new(),
        };

        let board_view = BoardView::new(
            reply_body.into_board_state(),
            window.input_state().create_delegate(),
            net.clone(),
            local_players,
            render_data.transform.clone(),
            render_data.camera.clone(),
        );

        self.set_board_view(board_view);

        Ok(())
    }

    fn close(
        &self,
        net: &Arc<Networking<GnsClientDriver>>,
    ) {
        self.clear_board_view();
        net.close_blocking();
        self.server.shutdown();
    }
}

#[async_trait(?Send)]
impl Application for ReversiClient {
    type ApplicationDriver = WinitDriver;
    type NetworkingDriver = GnsClientDriver;

    async fn init(&self, engine: &Arc<Engine<Self>>) {
        let mut cmd_registry = self.console.registry();

        let window = engine.window_manager().create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Reversi"),
            ..Default::default()
        }).await;

        let console_font = engine.resources().get_with_loader(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
        );
        let model_tile = engine.resources().get_with_loader(
            "tile.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().device().clone()),
        );
        let model_piece = engine.resources().get_with_loader(
            "piece.obj",
            ModelObjLoader::<ModelVertexNormal>::new(window.renderer().device().clone()),
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
                move |event: &mut Event<RendererStaleEvent>| {
                    camera.write().update(event.target());
                }
            ).unwrap();
        }

        let deferred_lighting = DeferredLightingRendererBootstrap::new(
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

        let render_data = RenderData {
            model_tile: model_tile.await.unwrap(),
            model_piece: model_piece.await.unwrap(),
            font: console_font,
            transform,
            camera,
            deferred_lighting,
            console_gui,
        };

        cmd_registry.register_command("host", Box::new(HostCommand {
            client: engine.clone(),
        })).unwrap();
        cmd_registry.register_command("connect", Box::new(ConnectCommand {
            client: engine.clone(),
        })).unwrap();
        cmd_registry.register_command("close", Box::new(CloseCommand {
            client: engine.clone(),
        })).unwrap();
        cmd_registry.register_command("name", Box::new(NameCommand {
            client: engine.clone(),
        })).unwrap();

        self.window.set(window).unwrap();
        self.render_data.set(render_data).unwrap();

        self.clear_board_view();
    }
}

fn parse_socket_addr(input: impl AsRef<str>) -> Result<SocketAddr, CommandError> {
    let input = input.as_ref();
    if let Ok(addr) = input.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = input.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, REVERSI_PORT));
    }
    if let Ok(port) = input.parse::<u16>() {
        return Ok(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port));
    }
    Err(CommandError::InvalidParameter(input.to_string()))
}

#[derive(Educe)]
#[educe(Debug)]
struct HostCommand {
    #[educe(Debug(ignore))]
    client: Arc<Engine<ReversiClient>>,
}

impl Command for HostCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::OneOf(vec![
            ParamCountCheck::Equal(0),
            ParamCountCheck::Equal(1),
        ]).check(parameters.len() as u32)?;

        let server = &self.client.app().server;

        let socket_addr = match parameters.len() {
            1 => parse_socket_addr(&parameters[0])?,
            _ => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), REVERSI_PORT),
        };

        server.restart(socket_addr)
            .map_err(|err| CommandError::CommandFailure(Box::new(err)))?;

        self.client.app().connect(
            socket_addr,
            self.client.net(),
        ).map_err(|err| CommandError::CommandFailure(Box::new(err)))
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct ConnectCommand {
    #[educe(Debug(ignore))]
    client: Arc<Engine<ReversiClient>>,
}

impl Command for ConnectCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::OneOf(vec![
            ParamCountCheck::Equal(0),
            ParamCountCheck::Equal(1),
        ]).check(parameters.len() as u32)?;

        let server = &self.client.app().server;

        let socket_addr = match parameters.len() {
            1 => parse_socket_addr(&parameters[0])?,
            _ => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), REVERSI_PORT),
        };

        server.shutdown();

        self.client.app().connect(
            socket_addr,
            self.client.net(),
        ).map_err(|err| CommandError::CommandFailure(Box::new(err)))
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct CloseCommand {
    #[educe(Debug(ignore))]
    client: Arc<Engine<ReversiClient>>,
}

impl Command for CloseCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(0).check(parameters.len() as u32)?;
        self.client.app().close(self.client.net());
        Ok(())
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct NameCommand {
    #[educe(Debug(ignore))]
    client: Arc<Engine<ReversiClient>>,
}

impl Command for NameCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(1).check(parameters.len() as u32)?;
        self.client.app().set_preferred_name(&parameters[0]);
        Ok(())
    }
}