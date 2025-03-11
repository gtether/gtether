use bitcode::{Decode, Encode};
use itertools::Itertools;
use std::collections::HashMap;

use crate::player::PlayerInfo;

pub mod controller;
pub mod view;

#[derive(Encode, Decode, Debug, Clone, Copy, Eq, PartialEq)]
pub enum GameState {
    InProgress,
    Draw,
    PlayerWon(usize),
}

#[derive(Encode, Decode, Debug, Clone, Copy)]
pub struct Tile {
    pos: [usize; 2],
    owner: Option<usize>,
}

impl Tile {
    fn new(pos: glm::TVec2<usize>) -> Self {
        Self {
            pos: pos.into(),
            owner: None,
        }
    }

    #[inline]
    pub fn pos(&self) -> glm::TVec2<usize> {
        self.pos.into()
    }

    #[inline]
    pub fn owner(&self) -> Option<usize> {
        self.owner
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    #[inline]
    pub fn offset(&self) -> glm::TVec2<i64> {
        match self {
            Self::Up => glm::vec2(0, 1),
            Self::Down => glm::vec2(0, -1),
            Self::Left => glm::vec2(-1, 0),
            Self::Right => glm::vec2(1, 0),
        }
    }

    #[inline]
    pub fn all() -> [Direction; 4] {
        [
            Self::Up,
            Self::Down,
            Self::Left,
            Self::Right,
        ]
    }

    #[inline]
    pub fn iter(self, current_pos: glm::TVec2<usize>, size: glm::TVec2<usize>) -> DirectionIter {
        DirectionIter {
            direction: self,
            current_pos,
            size,
        }
    }
}

pub struct DirectionIter {
    direction: Direction,
    current_pos: glm::TVec2<usize>,
    size: glm::TVec2<usize>,
}

impl Iterator for DirectionIter {
    type Item = glm::TVec2<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_pos.x < self.size.x && self.current_pos.y < self.size.y {
            let new_pos = self.current_pos.cast::<i64>() + self.direction.offset();
            if new_pos.x < 0 || new_pos.y < 0 || new_pos.x >= self.size.x as i64 || new_pos.y >= self.size.y as i64 {
                None
            } else {
                self.current_pos = (self.current_pos.cast::<i64>() + self.direction.offset())
                    .try_cast::<usize>()?;
                Some(self.current_pos)
            }
        } else {
            None
        }
    }
}

#[derive(Encode, Decode, Debug, Clone)]
pub struct BoardState {
    version: usize,
    size: [usize; 2],
    tiles: Vec<Tile>,
    players: Vec<PlayerInfo>,
    current_player_idx: usize,
    turn_no: usize,
    valid_moves_cache: Vec<[usize; 2]>,
    score_cache: HashMap<usize, usize>,
    game_state: GameState,
}

impl BoardState {
    pub fn new(size: glm::TVec2<usize>) -> Self {
        let tiles = (0..size.y).map(move |y| {
            (0..size.x).map(move |x| {
                Tile::new(glm::vec2(x, y))
            })
        }).flatten().collect::<Vec<_>>();

        let mut state = Self {
            version: 0,
            size: size.into(),
            tiles,
            players: vec![],
            current_player_idx: 0,
            turn_no: 1,
            valid_moves_cache: Vec::new(),
            score_cache: HashMap::new(),
            game_state: GameState::InProgress,
        };
        state.update_valid_moves_cache();
        state.update_score_cache();

        state
    }

    #[inline]
    pub fn version(&self) -> usize {
        self.version
    }

    #[inline]
    pub fn size(&self) -> glm::TVec2<usize> {
        self.size.into()
    }

    #[inline]
    pub fn tile(&self, pos: glm::TVec2<usize>) -> Option<&Tile> {
        self.tiles.get(pos.x + (pos.y * self.size().x))
    }

    fn tile_mut(&mut self, pos: glm::TVec2<usize>) -> Option<&mut Tile> {
        let size = self.size();
        self.tiles.get_mut(pos.x + (pos.y * size.x))
    }

    pub fn valid_selection(&self, pos: glm::TVec2<usize>) -> bool {
        if self.game_state != GameState::InProgress {
            // Game has already ended
            return false;
        }

        if let Some(tile) = self.tile(pos) {
            if tile.owner.is_some() {
                // Obviously can't play on a tile that is already owned
                return false;
            }
        } else {
            // Also can't play on a tile that doesn't exist
            return false;
        }

        if self.turn_no <= 4 {
            let size = self.size();
            // During first 4 turns, only center 4 spaces are valid
            let center = glm::vec2(size.x / 2, size.y / 2);
            if pos.x != center.x && pos.x != (center.x - 1) {
                return false;
            }
            if pos.y != center.y && pos.y != (center.y - 1) {
                return false;
            }
            true
        } else {
            // After the first 4 turns, every move must capture at least one piece
            Direction::all().into_iter().any(|direction| {
                self.valid_for_flip(pos, direction, self.current_player_idx)
            })
        }
    }

    pub fn set_players(&mut self, players: Vec<PlayerInfo>) {
        self.players = players;
        self.version += 1;
    }

    #[inline]
    pub fn player_info(&self, player_idx: usize) -> Option<&PlayerInfo> {
        self.players.get(player_idx)
    }

    #[inline]
    pub fn current_player_idx(&self) -> usize {
        self.current_player_idx
    }

    fn update_valid_moves_cache(&mut self) {
        self.valid_moves_cache.clear();
        let size = self.size();
        for x in 0..size.x {
            for y in 0..size.y {
                let pos = glm::vec2(x, y);
                if self.valid_selection(pos) {
                    self.valid_moves_cache.push(pos.into());
                }
            }
        }
    }

    #[inline]
    pub fn valid_moves(&self) -> impl ExactSizeIterator<Item=glm::TVec2<usize>> + use<'_> {
        self.valid_moves_cache.iter()
            .map(|v| v.clone().into())
    }

    fn update_score_cache(&mut self) {
        self.score_cache.clear();
        let size = self.size();
        for x in 0..size.x {
            for y in 0..size.y {
                if let Some(tile) = self.tile(glm::vec2(x, y)) {
                    if let Some(owner) = tile.owner {
                        *self.score_cache.entry(owner).or_default() += 1;
                    }
                }
            }
        }
    }

    #[inline]
    pub fn score(&self, player_idx: usize) -> usize {
        self.score_cache.get(&player_idx).cloned().unwrap_or_default()
    }

    #[inline]
    pub fn total_pieces(&self) -> usize {
        // Total count of pieces is equivalent to sum of all player scores
        (0..self.players.len())
            .map(|player_idx| self.score(player_idx))
            .sum()
    }

    pub fn end_turn(&mut self) {
        self.current_player_idx += 1;
        if self.current_player_idx >= self.players.len() {
            self.current_player_idx = 0;
        }
        self.turn_no += 1;
        self.update_valid_moves_cache();
        self.update_score_cache();
        if self.valid_moves_cache.is_empty() {
            self.end_game();
        } else {
            self.version += 1;
        }
    }

    pub fn end_game(&mut self) {
        let mut high_scores = (0..self.players.len())
            .map(|player_idx| (player_idx, self.score(player_idx)))
            .max_set_by(|(_, count_a), (_, count_b)| {
                count_a.cmp(count_b)
            });
        if high_scores.len() == 1 {
            let (player_idx, _) = high_scores.pop().unwrap();
            self.game_state = GameState::PlayerWon(player_idx);
        } else {
            self.game_state = GameState::Draw;
        }
        self.version += 1;
    }

    #[inline]
    pub fn game_state(&self) -> GameState {
        self.game_state.clone()
    }

    fn valid_for_flip(&self, start_pos: glm::TVec2<usize>, direction: Direction, player_idx: usize) -> bool {
        let mut pieces_flipped: usize = 0;
        for current_pos in direction.iter(start_pos, self.size()) {
            if let Some(tile) = self.tile(current_pos) {
                if let Some(owner) = tile.owner {
                    if owner == player_idx {
                        // Found same owner, all tiles in-between are valid for flipping, if there
                        // are any
                        return pieces_flipped > 0;
                    } else {
                        // Else it's some other owned tile, continue searching
                        pieces_flipped += 1;
                    }
                } else {
                    // Found unowned space, not valid for a flip
                    return false;
                }
            } else {
                // Hit edge of board, not valid for a flip
                return false;
            }
        }
        // Hit edge of board, not valid for a flip
        false
    }

    fn flip(&mut self, start_pos: glm::TVec2<usize>, direction: Direction, player_idx: usize) {
        for current_pos in direction.iter(start_pos, self.size()) {
            if let Some(tile) = self.tile_mut(current_pos) {
                if let Some(owner) = tile.owner {
                    if owner == player_idx {
                        // Found same owner, reached end of flipping
                        return;
                    } else {
                        tile.owner = Some(player_idx);
                    }
                } else {
                    // No owner, stop flipping
                    return;
                }
            } else {
                // Hit edge of board, stop flipping
                return;
            }
        }
    }

    pub fn set(&mut self, pos: glm::TVec2<usize>, player_idx: usize) -> bool {
        if player_idx >= self.players.len() {
            return false;
        }

        if let Some(tile) = self.tile_mut(pos) {
            tile.owner = Some(player_idx);
        } else {
            return false;
        }

        for direction in Direction::all() {
            if self.valid_for_flip(pos, direction, player_idx) {
                self.flip(pos, direction, player_idx);
            }
        }

        self.version += 1;

        true
    }

    #[inline]
    pub fn place(&mut self, pos: glm::TVec2<usize>) -> bool {
        if self.valid_selection(pos) {
            self.set(pos, self.current_player_idx)
        } else {
            false
        }
    }

    pub fn iter(&self) -> impl Iterator<Item=&Tile> {
        self.tiles.iter()
    }
}