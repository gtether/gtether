// Cloned from:
// https://github.com/PLURedTeam/reversi/blob/master/core/src/main/java/plu/red/reversi/core/game/ReversiMinimax.java

use std::cmp::{max, min};
use std::sync::Arc;
use glm::TVec2;

use crate::board::{Board, BoardState};
use crate::bot::BotAlgorithm;

#[derive(Debug)]
pub struct MinimaxAlgorithm {
    max_depth: usize,
}

impl MinimaxAlgorithm {
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
        }
    }

    fn heuristic_score(&self, board: &BoardState, player_idx: usize, endgame: bool) -> i64 {
        // Heuristic score is the different between our score and everyone else's score
        //  other = (all - ours)
        //  ours - other == ours * 2 - all
        let mut score = (board.score(player_idx) * 2) as i64 - board.total_pieces() as i64;

        if !endgame {
            // Prioritize grabbing the corners, as they are some of the best spots
            let corners = [
                glm::vec2(0, 0),
                glm::vec2(0, board.size().y - 1),
                glm::vec2(board.size().x - 1, 0),
                glm::vec2(board.size().x - 1, board.size().y - 1),
            ];
            for corner in corners {
                if let Some(tile) = board.tile(corner) {
                    if let Some(owner) = tile.owner() {
                        if owner == player_idx {
                            score += 4;
                        } else {
                            score -= 4;
                        }
                    }
                }
            }
        }

        // Weight the score
        score * 16
    }

    fn find_best_play_score(
        &self,
        board: &BoardState,
        player_idx: usize,
        mut alpha: i64,
        mut beta: i64,
        depth: usize,
    ) -> i64 {
        if depth >= self.max_depth {
            return self.heuristic_score(board, player_idx, false);
        }

        let valid_moves = board.valid_moves();
        if valid_moves.len() == 0 {
            return self.heuristic_score(board, player_idx, true);
        }

        let maximize = board.current_player_idx() == player_idx;
        let mut best_score = if maximize { i64::MIN } else { i64::MAX };

        for possible_move in valid_moves {
            let mut sub_board = board.clone();

            assert!(sub_board.place(possible_move.clone()));
            sub_board.end_turn();

            let child_score = self.find_best_play_score(
                &sub_board,
                player_idx,
                alpha,
                beta,
                depth + 1,
            );
            if maximize && child_score > best_score {
                best_score = child_score;
                alpha = max(alpha, child_score);
            } else if !maximize && child_score < best_score {
                best_score = child_score;
                beta = min(beta, child_score);
            }

            if beta <= alpha { break }
        }

        best_score
    }
}

impl BotAlgorithm for MinimaxAlgorithm {
    fn find_best_play(&self, board: &Arc<Board>) -> Option<TVec2<usize>> {
        let board_state = board.state();
        let my_player_idx = board_state.current_player_idx();
        board_state.valid_moves()
            .map(|possible_move| {
                let mut sub_board = board_state.clone();

                assert!(sub_board.place(possible_move.clone()));
                sub_board.end_turn();

                let child_score = self.find_best_play_score(
                    &sub_board,
                    my_player_idx,
                    i64::MIN,
                    i64::MAX,
                    1,
                );
                (possible_move.clone(), child_score)
            })
            .max_by(|(_, score_a), (_, score_b)| {
                score_a.cmp(score_b)
            })
            .map(|(possible_move, _)| possible_move)
    }
}