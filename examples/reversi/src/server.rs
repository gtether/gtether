use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use async_trait::async_trait;
use bitcode::{Decode, Encode};
use educe::Educe;
use gtether::net::gns::GnsSubsystem;
use gtether::net::message::server::ServerMessageHandler;
use gtether::net::message::{Message, MessageBody};
use gtether::net::server::{Connection, ServerNetworking, ServerNetworkingDisconnectEvent, ServerNetworkingError};
use gtether::server::{Server, ServerBuildError};
use gtether::{Application, Engine, EngineBuilder, EngineJoinHandle};
use parking_lot::{Mutex, RwLock};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{debug, info};
use gtether::event::{Event, EventHandler};
use gtether::net::NetworkingBuildError;
use crate::board::controller::BoardController;
use crate::board::BoardState;
use crate::bot::minimax::MinimaxAlgorithm;
use crate::player::Player;
use crate::render_util::hsv_to_rgb;

pub const REVERSI_PORT: u16 = 19502;

#[derive(Encode, Decode, MessageBody, Debug)]
#[message_flag(Reliable)]
#[message_reply(PlayerConnectReply)]
pub struct PlayerConnect {
    requested_name: String,
    color: Option<[f32; 3]>,
}

impl PlayerConnect {
    #[inline]
    pub fn new(
        requested_name: impl Into<String>,
        color: Option<glm::TVec3<f32>>,
    ) -> Self {
        Self {
            requested_name: requested_name.into(),
            color: color.map(Into::into),
        }
    }

    #[inline]
    pub fn requested_name(&self) -> &str {
        &self.requested_name
    }

    #[inline]
    pub fn color(&self) -> Option<glm::TVec3<f32>> {
        self.color.clone().map(Into::into)
    }
}

#[derive(Encode, Decode, MessageBody, Debug)]
#[message_flag(Reliable)]
pub struct PlayerConnectReply {
    name: String,
    player_idx: Option<usize>,
    board_state: BoardState,
}

impl PlayerConnectReply {
    #[inline]
    pub fn new_player(
        name: impl Into<String>,
        player_idx: usize,
        board_state: BoardState,
    ) -> Self {
        Self {
            name: name.into(),
            player_idx: Some(player_idx),
            board_state,
        }
    }

    #[inline]
    pub fn new_spectator(
        name: impl Into<String>,
        board_state: BoardState,
    ) -> Self {
        Self {
            name: name.into(),
            player_idx: None,
            board_state,
        }
    }

    #[allow(unused)]
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn player_idx(&self) -> Option<usize> {
        self.player_idx.clone()
    }

    #[inline]
    pub fn into_board_state(self) -> BoardState {
        self.board_state
    }
}

#[derive(Debug)]
struct RemotePlayer {
    player_idx: usize,
    player: Arc<Player>,
}

#[derive(Debug)]
struct Spectator {
    name: String,
}

#[derive(Debug)]
struct PlayerManagerState {
    players: HashMap<Connection, RemotePlayer>,
    spectators: HashMap<Connection, Spectator>,
}

#[derive(Educe)]
#[educe(Debug)]
struct PlayerManager {
    state: RwLock<PlayerManagerState>,
    board_controller: Arc<BoardController>,
    #[educe(Debug(ignore))]
    net: Arc<ServerNetworking>,
}

impl PlayerManager {
    fn new(
        board_controller: Arc<BoardController>,
        net: &Arc<ServerNetworking>,
    ) -> Arc<Self> {
        let state = RwLock::new(PlayerManagerState {
            players: HashMap::new(),
            spectators: HashMap::new(),
        });

        let player_manager = Arc::new(Self {
            state,
            board_controller,
            net: net.clone(),
        });

        net.insert_msg_handler(Arc::downgrade(&player_manager));
        net.event_bus().register(player_manager.clone()).unwrap();

        player_manager
    }

    fn check_name(state: &PlayerManagerState, name: &str) -> bool {
        for player in state.players.values() {
            if name == player.player.info().name {
                return false;
            }
        }

        for spectator in state.spectators.values() {
            if name == &spectator.name {
                return false;
            }
        }

        true
    }

    fn default_color_for_player(player_idx: usize) -> glm::TVec3<f32> {
        if player_idx == 0 {
            // First player is white
            glm::vec3(0.05, 0.05, 0.05)
        } else if player_idx == 1 {
            // Second player is black
            glm::vec3(0.95, 0.95, 0.95)
        } else {
            // Others (non-standard) can be rainbow

            // Use hue chunks of 45 degrees
            let mut hue = player_idx * 45;
            // Offset by 5 for every time we've done a full hue rotation, for slightly varied colors
            hue = (hue + (5 * (hue / 360))) % 360;

            hsv_to_rgb(glm::vec3(hue as f32, 1.0, 0.95))
        }
    }
}

impl ServerMessageHandler<PlayerConnect, ServerNetworkingError> for PlayerManager {
    fn handle(&self, connection: Connection, msg: Message<PlayerConnect>) -> Result<(), ServerNetworkingError> {
        let mut state = self.state.write();
        let msg_body = msg.body();

        let name = if Self::check_name(&state, msg_body.requested_name()) {
            msg_body.requested_name().to_owned()
        } else {
            let mut idx = 0;
            loop {
                idx += 1;
                let name = msg_body.requested_name().to_owned() + &idx.to_string();
                if Self::check_name(&state, &name) {
                    break name
                }
            }
        };

        for (player_idx, player) in self.board_controller.players().into_iter().enumerate() {
            if player.player_type().replaceable() {
                let color = msg_body.color()
                    .unwrap_or(Self::default_color_for_player(state.players.len()));
                let new_player = Arc::new(Player::human(name.clone(), color));
                state.players.insert(connection, RemotePlayer {
                    player_idx,
                    player: new_player.clone(),
                });
                self.board_controller.replace_player(player_idx, new_player, [connection].into());

                let reply = msg.reply(PlayerConnectReply::new_player(
                    name,
                    player_idx,
                    self.board_controller.board().clone(),
                ));
                self.net.send(connection, reply)?;

                return Ok(());
            }
        }

        state.spectators.insert(connection, Spectator {
            name: name.clone(),
        });

        let reply = msg.reply(PlayerConnectReply::new_spectator(
            name,
            self.board_controller.board().clone(),
        ));
        self.net.send(connection, reply)?;

        Ok(())
    }
}

impl EventHandler<ServerNetworkingDisconnectEvent> for PlayerManager {
    fn handle_event(&self, event: &mut Event<ServerNetworkingDisconnectEvent>) {
        let mut state = self.state.write();

        if let Some(remote_player) = state.players.remove(&event.connection()) {
            let name = remote_player.player.info().name.clone();
            info!(connection = ?event.connection(), ?name, "Player disconnected");
            // TODO: configure bot replacement?
            let bot_player = Arc::new(Player::bot(
                format!("Bot{}", remote_player.player_idx),
                Self::default_color_for_player(remote_player.player_idx),
                MinimaxAlgorithm::new(5),
            ));
            self.board_controller.replace_player(
                remote_player.player_idx,
                bot_player,
                HashSet::new(),
            );
        }

        state.spectators.remove(&event.connection());
    }
}

pub struct ReversiServer {
    board_controller: OnceLock<Arc<BoardController>>,
    player_manager: OnceLock<Arc<PlayerManager>>,
}

impl ReversiServer {
    pub fn new() -> Self {
        Self {
            board_controller: OnceLock::new(),
            player_manager: OnceLock::new(),
        }
    }
}

#[async_trait(?Send)]
impl Application<Server> for ReversiServer {
    async fn init(&self, engine: &Arc<Engine<Self, Server>>) {
        let board_state = BoardState::new(
            glm::vec2(8, 8),
        );

        let board_controller = BoardController::new(
            board_state,
            vec![
                Arc::new(Player::bot(
                    "Bot1",
                    glm::vec3(0.05, 0.05, 0.05),
                    MinimaxAlgorithm::new(5),
                )),
                Arc::new(Player::bot(
                    "Bot2",
                    glm::vec3(0.95, 0.95, 0.95),
                    MinimaxAlgorithm::new(5),
                )),
            ],
            engine.side().net(),
        );

        let player_manager = PlayerManager::new(
            board_controller.clone(),
            engine.side().net(),
        );

        self.board_controller.set(board_controller)
            .expect("'board_controller' should not be set before initialization");
        self.player_manager.set(player_manager)
            .expect("'player_manager' should not be set before initialization");
    }

    fn tick(&self, _engine: &Arc<Engine<Self, Server>>, _delta: Duration) {
        /* noop */
    }
}

#[derive(Debug)]
pub struct ServerStartError {
    source: Option<Box<dyn Error>>,
}

impl Display for ServerStartError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            Some(source) => write!(f, "Failed to start server; {source}"),
            None => write!(f, "Failed to start server"),
        }
    }
}

impl Error for ServerStartError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|b| b.as_ref())
    }
}

impl From<NetworkingBuildError> for ServerStartError {
    #[inline]
    fn from(value: NetworkingBuildError) -> Self {
        Self {
            source: Some(Box::new(value)),
        }
    }
}

impl From<ServerBuildError> for ServerStartError {
    #[inline]
    fn from(value: ServerBuildError) -> Self {
        Self {
            source: Some(Box::new(value)),
        }
    }
}

pub struct ReversiServerManager {
    inner: Mutex<Option<EngineJoinHandle<ReversiServer, Server>>>,
}

impl ReversiServerManager {
    #[inline]
    pub fn new() -> Self {
        Self{
            inner: Mutex::new(None),
        }
    }

    fn shutdown_impl(inner: &mut Option<EngineJoinHandle<ReversiServer, Server>>) {
        if let Some(server) = inner.take() {
            info!("Stopping server...");
            server.stop().unwrap();
            info!("Server stopped.");
        } else {
            debug!("Server is already stopped");
        }
    }

    pub fn shutdown(&self) {
        Self::shutdown_impl(&mut self.inner.lock());
    }

    fn start_impl(
        inner: &mut Option<EngineJoinHandle<ReversiServer, Server>>,
    ) -> Result<(), ServerStartError> {
        info!("Starting server...");
        let app = ReversiServer::new();
        let networking = ServerNetworking::builder()
            .raw_factory(GnsSubsystem::get())
            .port(REVERSI_PORT)
            .build()?;
        let side = Server::builder()
            .networking(networking)
            .build()?;
        let join_handle = EngineBuilder::new()
            .app(app)
            .side(side)
            .spawn();
        *inner = Some(join_handle);
        info!("Server started.");
        Ok(())
    }

    #[allow(unused)]
    pub fn start(&self) -> Result<Arc<Engine<ReversiServer, Server>>, ServerStartError> {
        let mut inner = self.inner.lock();
        if inner.is_none() {
            Self::start_impl(&mut inner)?;
        } else {
            debug!("Server is already running");
        }
        Ok(inner.as_ref().unwrap().engine())
    }

    pub fn restart(&self) -> Result<Arc<Engine<ReversiServer, Server>>, ServerStartError> {
        let mut inner = self.inner.lock();
        Self::shutdown_impl(&mut inner);
        Self::start_impl(&mut inner)?;
        Ok(inner.as_ref().unwrap().engine())
    }
}