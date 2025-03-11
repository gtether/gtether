use std::sync::Arc;
use bitcode::{Decode, Encode};

use crate::board::controller::BoardController;
use crate::bot::{BotAlgorithm, BotRunner};

#[derive(Debug, Clone, Encode, Decode)]
pub struct PlayerInfo {
    pub name: String,
    pub color: [f32; 3],
}

impl PlayerInfo {
    #[inline]
    pub fn new(
        name: impl Into<String>,
        color: glm::TVec3<f32>,
    ) -> Self {
        Self {
            name: name.into(),
            color: color.into(),
        }
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn color(&self) -> glm::TVec3<f32> {
        self.color.into()
    }
}

impl PartialEq for PlayerInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for PlayerInfo {}

#[derive(Debug)]
pub enum PlayerType {
    Human,
    Bot(BotRunner),
}

impl PlayerType {
    #[inline]
    pub fn replaceable(&self) -> bool {
        match self {
            Self::Bot(_) => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct Player {
    info: PlayerInfo,
    p_type: PlayerType,
}

impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self.info == other.info
    }
}

impl Eq for Player {}

impl Player {
    #[inline]
    pub fn human(
        name: impl Into<String>,
        color: glm::TVec3<f32>,
    ) -> Self {
        Self {
            info: PlayerInfo::new(name, color),
            p_type: PlayerType::Human,
        }
    }

    #[inline]
    pub fn bot(
        name: impl Into<String>,
        color: glm::TVec3<f32>,
        algorithm: impl BotAlgorithm,
    ) -> Self {
        Self {
            info: PlayerInfo::new(name, color),
            p_type: PlayerType::Bot(BotRunner::new(algorithm)),
        }
    }

    #[inline]
    pub fn info(&self) -> &PlayerInfo {
        &self.info
    }

    #[inline]
    pub fn player_type(&self) -> &PlayerType {
        &self.p_type
    }

    pub fn begin_turn(&self, board: Arc<BoardController>) {
        match &self.p_type {
            PlayerType::Human => {},
            PlayerType::Bot(runner) => runner.play(board),
        }
    }
}