use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;

use crate::board::controller::BoardController;

pub mod minimax;

pub trait BotAlgorithm: Debug + Send + Sync + 'static {
    fn find_best_play(&self, board: &Arc<BoardController>) -> Option<glm::TVec2<usize>>;
}

#[derive(Debug)]
enum BotRunnerMsg {
    Play(Arc<BoardController>),
    Stop,
}

pub struct BotRunner {
    join_handle: Option<JoinHandle<()>>,
    client: ump::Client<BotRunnerMsg, (), ()>,
}

impl Debug for BotRunner {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotRunner")
            .field("join_handle", &self.join_handle)
            .field("client", &"...")
            .finish()
    }
}

impl BotRunner {
    pub fn new(algorithm: impl BotAlgorithm) -> Self {
        let (server, client) = ump::channel::<BotRunnerMsg, (), ()>();

        let join_handle = thread::spawn(move || {
            loop {
                let (msg, reply_ctx) = match server.wait() {
                    Ok(response) => response,
                    Err(ump::Error::ClientsDisappeared) => break,
                    Err(e) => panic!("Unexpected error: {e}"),
                };

                match reply_ctx.reply(()) {
                    Ok(_) => {},
                    Err(ump::Error::ClientsDisappeared) => break,
                    Err(e) => panic!("Unexpected error: {e}"),
                }

                match msg {
                    BotRunnerMsg::Play(board) => {
                        if let Some(best_move) = algorithm.find_best_play(&board) {
                            board.play(best_move);
                        }
                    },
                    BotRunnerMsg::Stop => {
                        break;
                    }
                }
            }
        });

        Self {
            join_handle: Some(join_handle),
            client
        }
    }

    pub fn play(&self, board: Arc<BoardController>) {
        match self.client.req(BotRunnerMsg::Play(board)) {
            Ok(_) => {},
            Err(ump::Error::ServerDisappeared | ump::Error::NoReply) => {},
            Err(e) => panic!("Unexpected error: {e}"),
        }
    }
}

impl Drop for BotRunner {
    fn drop(&mut self) {
        if let Some(join_handle) = self.join_handle.take() {
            match self.client.req(BotRunnerMsg::Stop) {
                Ok(_) => join_handle.join().unwrap(),
                Err(ump::Error::ServerDisappeared | ump::Error::NoReply) => {},
                Err(e) => panic!("Unexpected error: {e}"),
            }
        }
    }
}