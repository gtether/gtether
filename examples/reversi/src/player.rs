use std::sync::Arc;

use crate::board::Board;
use crate::bot::{BotAlgorithm, BotRunner};

#[derive(Debug)]
pub enum PlayerType {
    Human,
    Bot(BotRunner),
}

impl PlayerType {
    pub fn has_manual_input(&self) -> bool {
        match self {
            Self::Human => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct Player {
    name: String,
    color: glm::TVec3<f32>,
    p_type: PlayerType,
}

impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
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
            name: name.into(),
            color,
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
            name: name.into(),
            color,
            p_type: PlayerType::Bot(BotRunner::new(algorithm)),
        }
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn color(&self) -> glm::TVec3<f32> {
        self.color
    }

    #[inline]
    pub fn player_type(&self) -> &PlayerType {
        &self.p_type
    }

    pub fn begin_turn(&self, board: &Arc<Board>) {
        match &self.p_type {
            PlayerType::Human => {},
            PlayerType::Bot(runner) => runner.play(board.clone()),
        }
    }
}