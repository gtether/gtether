use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, Weak};
use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};
use bitcode::{Decode, Encode};
use educe::Educe;
use tracing::{info, warn};
use gtether::net::message::{Message, MessageBody};
use gtether::net::message::MessageHandler;
use gtether::net::{Connection, Networking};
use gtether::net::gns::GnsServerDriver;

use crate::board::{BoardState, GameState};
use crate::board::view::MessageUpdateBoard;
use crate::player::Player;

#[derive(Encode, Decode, MessageBody, Debug)]
#[message_flag(Reliable)]
pub struct MessagePlay {
    player_idx: usize,
    pos: [usize; 2],
}

impl MessagePlay {
    #[inline]
    pub fn new(
        player_idx: usize,
        pos: glm::TVec2<usize>,
    ) -> Self {
        Self {
            player_idx,
            pos: pos.into(),
        }
    }

    #[inline]
    pub fn player_idx(&self) -> usize {
        self.player_idx
    }

    #[inline]
    pub fn pos(&self) -> glm::TVec2<usize> {
        self.pos.into()
    }
}

#[derive(Debug)]
struct PlayerState {
    player: Arc<Player>,
    valid_connections: HashSet<Connection>,
}

#[derive(Debug)]
struct BoardControllerState {
    board: BoardState,
    players: Vec<PlayerState>,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct BoardController {
    weak: Weak<Self>,
    state: RwLock<BoardControllerState>,
    #[educe(Debug(ignore))]
    net: Arc<Networking<GnsServerDriver>>,
}

impl BoardController {
    pub fn new(
        mut board: BoardState,
        players: Vec<Arc<Player>>,
        net: &Arc<Networking<GnsServerDriver>>,
    ) -> Arc<Self> {
        board.set_players(players.iter()
            .map(|p| p.info().clone())
            .collect());

        let players = players.into_iter()
            .map(|player| PlayerState {
                player,
                valid_connections: HashSet::new(),
            })
            .collect();

        let state = RwLock::new(BoardControllerState {
            board,
            players,
        });

        let board_controller = Arc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            state,
            net: net.clone(),
        });

        net.insert_msg_handler(Arc::downgrade(&board_controller));

        board_controller
    }

    #[inline]
    pub fn board(&self) -> MappedRwLockReadGuard<'_, BoardState> {
        RwLockReadGuard::map(
            self.state.read(),
            |state| &state.board
        )
    }

    fn update_clients(&self, state: &BoardControllerState) {
        let msg = MessageUpdateBoard::new(state.board.clone());
        match self.net.broadcast(msg) {
            Ok(_) => {},
            Err(error) =>
                warn!(?error, "Failed to broadcast MessageUpdateBoard"),
        }
    }

    pub fn play(&self, pos: glm::TVec2<usize>) {
        let self_ref = self.weak.upgrade().unwrap();
        let next_player = {
            let mut state = self.state.write();
            if state.board.place(pos) {
                state.board.end_turn();

                self.update_clients(&state);

                if state.board.game_state == GameState::InProgress {
                    let next_player_idx = state.board.current_player_idx();
                    let next_player_info = state.board.player_info(next_player_idx).unwrap();
                    info!(
                        turn_no = state.board.turn_no,
                        player_idx = next_player_idx,
                        player_info = ?next_player_info,
                        "Begin turn");
                    state.players.get(next_player_idx)
                        .map(|p| p.player.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(next_player) = next_player {
            next_player.begin_turn(self_ref);
        }
    }

    pub fn players(&self) -> Vec<Arc<Player>> {
        let state = self.state.read();
        state.players.iter()
            .map(|p| p.player.clone())
            .collect()
    }

    pub fn replace_player(
        &self,
        player_idx: usize,
        player: Arc<Player>,
        valid_connections: HashSet<Connection>,
    ) {
        let mut state = self.state.write();
        state.players[player_idx] = PlayerState {
            player: player.clone(),
            valid_connections,
        };
        let players = state.players.iter()
            .map(|p| p.player.info().clone())
            .collect();
        state.board.set_players(players);
        self.update_clients(&state);

        if player_idx == state.board.current_player_idx() {
            drop(state);
            player.begin_turn(self.weak.upgrade().unwrap());
        }
    }
}

impl MessageHandler<MessagePlay, Infallible> for BoardController {
    fn handle(&self, connection: Connection, msg: Message<MessagePlay>) -> Result<(), Infallible> {
        let msg = msg.into_body();

        {
            let state = self.state.read();

            if msg.player_idx() != state.board.current_player_idx() {
                warn!(
                    ?connection,
                    player_idx = ?msg.player_idx(),
                    "Ignoring MessageUpdateBoard for non-current player",
                );
                return Ok(())
            }

            let player = &state.players[msg.player_idx()];
            if !player.valid_connections.contains(&connection) {
                warn!(
                    ?connection,
                    valid_connections = ?player.valid_connections,
                    player_idx = ?msg.player_idx(),
                    "Ignoring MessageUpdateBoard for invalid connection",
                );
                return Ok(())
            }
        }

        self.play(msg.pos());

        Ok(())
    }
}