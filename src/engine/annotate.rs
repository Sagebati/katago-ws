//! Turn KataGo per-turn analysis into the annotation payload returned to clients.
//!
//! Win-rate loss for move *i* is computed from consecutive root evaluations:
//! `loss = winrate(turn i)  −  (1 − winrate(turn i+1))`, i.e. the mover's best
//! achievable win-rate minus their actual resulting win-rate. This is robust
//! even when the played move isn't among KataGo's reported candidates.

use std::collections::HashMap;

use serde::Serialize;

use crate::engine::katago::TurnResponse;
use crate::engine::sgf::{Color, ParsedGame, Rules};

/// How a played move is judged, by either lens (win-rate loss or score loss).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Negligible loss — essentially the best move.
    Good,
    /// A small slip.
    Inaccuracy,
    /// A clear error.
    Mistake,
    /// A large, game-affecting error.
    Blunder,
}

/// A candidate move KataGo suggested for a position.
#[derive(Serialize)]
pub struct Candidate {
    /// Move in GTP notation.
    pub mv: String,
    /// Win probability after this move (mover's perspective).
    pub winrate: f32,
    /// Score lead after this move.
    pub score_lead: f32,
    /// Raw policy prior.
    pub prior: f32,
    /// MCTS visits.
    pub visits: u32,
    /// Principal variation following this move.
    pub pv: Vec<String>,
}

/// Annotation for a single played move.
#[derive(Serialize)]
pub struct MoveAnnotation {
    /// 1-based move number.
    pub move_number: usize,
    /// Mover's colour.
    pub color: Color,
    /// The move played, GTP notation.
    pub mv: String,
    /// Mover's win-rate after the actual move.
    pub winrate: f32,
    /// Best win-rate achievable from this position.
    pub best_winrate: f32,
    /// `best_winrate − winrate`, clamped at 0.
    pub winrate_loss: f32,
    /// Classification by win-rate loss.
    pub winrate_classification: Classification,
    /// Expected score lead (mover's perspective) before the move — the best
    /// achievable with optimal play.
    pub score_lead: f32,
    /// Points given up by the move: `score_lead − resulting_lead`, clamped at 0.
    pub score_loss: f32,
    /// Classification by score loss (points). A more uniform yardstick than
    /// win-rate across game phases.
    pub score_classification: Classification,
    /// Top candidate moves for this position.
    pub top_moves: Vec<Candidate>,
}

/// Mistake/blunder counts under one lens (win-rate loss or score loss).
#[derive(Serialize, Default)]
pub struct Tally {
    /// Black moves classified as `mistake`.
    pub black_mistakes: usize,
    /// White moves classified as `mistake`.
    pub white_mistakes: usize,
    /// Black moves classified as `blunder`.
    pub black_blunders: usize,
    /// White moves classified as `blunder`.
    pub white_blunders: usize,
}

impl Tally {
    /// Count one move's classification against the mover's colour.
    fn record(&mut self, classification: Classification, color: Color) {
        match (classification, color) {
            (Classification::Mistake, Color::Black) => self.black_mistakes += 1,
            (Classification::Mistake, Color::White) => self.white_mistakes += 1,
            (Classification::Blunder, Color::Black) => self.black_blunders += 1,
            (Classification::Blunder, Color::White) => self.white_blunders += 1,
            _ => {}
        }
    }
}

/// Whole-game roll-up, tallied under both lenses (see [`Tally`]).
#[derive(Serialize)]
pub struct GameSummary {
    /// Number of annotated moves.
    pub num_moves: usize,
    /// Counts by win-rate loss (probability dropped).
    pub by_winrate: Tally,
    /// Counts by score loss (points dropped).
    pub by_score: Tally,
}

/// Full analysis payload.
#[derive(Serialize)]
pub struct GameAnalysis {
    /// Board size.
    pub board_size: u8,
    /// Komi.
    pub komi: f32,
    /// KataGo rules used.
    pub rules: Rules,
    /// Raw SGF result, if any.
    pub result: Option<String>,
    /// Per-move annotations in play order.
    pub moves: Vec<MoveAnnotation>,
    /// Aggregate summary.
    pub summary: GameSummary,
}

/// Classify a move by win-rate loss (win probability the move gave up).
fn classify_winrate(loss: f32) -> Classification {
    if loss < 0.03 {
        Classification::Good
    } else if loss < 0.07 {
        Classification::Inaccuracy
    } else if loss < 0.15 {
        Classification::Mistake
    } else {
        Classification::Blunder
    }
}

/// Classify a move by score loss (points the move gave up). Heuristic
/// thresholds in points — tune to taste. Points are a steadier yardstick than
/// win-rate across game phases: a win-rate near 0/1 hides real point losses in
/// a decided game, while an even game inflates tiny slips. (This is why OGS
/// offers a score-based review alongside the win-rate one.)
fn classify_score(points_lost: f32) -> Classification {
    if points_lost < 1.0 {
        Classification::Good
    } else if points_lost < 3.0 {
        Classification::Inaccuracy
    } else if points_lost < 6.0 {
        Classification::Mistake
    } else {
        Classification::Blunder
    }
}

/// Assemble the analysis from the parsed game and KataGo's per-turn results.
pub fn assemble(game: &ParsedGame, turns: Vec<TurnResponse>, top_k: usize) -> GameAnalysis {
    let by_turn: HashMap<usize, TurnResponse> = turns
        .into_iter()
        .map(|turn| (turn.turn_number, turn))
        .collect();

    let mut moves = Vec::with_capacity(game.moves.len());
    let mut summary = GameSummary {
        num_moves: game.moves.len(),
        by_winrate: Tally::default(),
        by_score: Tally::default(),
    };

    for (i, (color, mv)) in game.moves.iter().enumerate() {
        let cur = by_turn.get(&i).and_then(|turn| turn.root_info.as_ref());
        let next = by_turn
            .get(&(i + 1))
            .and_then(|turn| turn.root_info.as_ref());

        let best_winrate = cur.map(|root| root.winrate).unwrap_or(0.5);
        // After the move it's the opponent to move, so mover's winrate = 1 - theirs.
        let winrate = next.map(|root| 1.0 - root.winrate).unwrap_or(best_winrate);
        let winrate_loss = (best_winrate - winrate).max(0.0);
        let winrate_classification = classify_winrate(winrate_loss);

        // Score lead is zero-sum: after the move the opponent is to play, so the
        // mover's resulting lead is the negation of the opponent's eval.
        let best_score = cur.map(|root| root.score_lead).unwrap_or(0.0);
        let resulting_score = next.map(|root| -root.score_lead).unwrap_or(best_score);
        let score_loss = (best_score - resulting_score).max(0.0);
        let score_classification = classify_score(score_loss);

        let top_moves = by_turn
            .get(&i)
            .map(|turn| {
                let mut infos: Vec<&_> = turn.move_infos.iter().collect();
                infos.sort_by_key(|info| info.order);
                infos
                    .into_iter()
                    .take(top_k)
                    .map(|info| Candidate {
                        mv: info.mv.clone(),
                        winrate: info.winrate,
                        score_lead: info.score_lead,
                        prior: info.prior,
                        visits: info.visits,
                        pv: info.pv.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        summary.by_winrate.record(winrate_classification, *color);
        summary.by_score.record(score_classification, *color);

        moves.push(MoveAnnotation {
            move_number: i + 1,
            color: *color,
            mv: mv.clone(),
            winrate,
            best_winrate,
            winrate_loss,
            winrate_classification,
            score_lead: best_score,
            score_loss,
            score_classification,
            top_moves,
        });
    }

    GameAnalysis {
        board_size: game.board_size,
        komi: game.komi,
        rules: game.rules,
        result: game.result.clone(),
        moves,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game_one_move() -> ParsedGame {
        ParsedGame {
            board_size: 19,
            komi: 7.5,
            rules: Rules::Japanese,
            result: None,
            initial_stones: vec![],
            moves: vec![(Color::Black, "Q16".to_owned())],
        }
    }

    #[test]
    fn computes_loss_from_consecutive_turns() {
        // Turn 0 (Black to move): best winrate 0.60, best score lead +2.0.
        // Turn 1 (White to move): White winrate 0.55 → Black's actual = 0.45;
        //   White score lead +1.0 → Black's resulting lead = -1.0.
        // winrate loss = 0.60 - 0.45 = 0.15 → blunder.
        // score loss   = 2.0 - (-1.0) = 3.0 → mistake.
        // (Same move, two lenses, different verdicts — the whole point.)
        let turns: Vec<TurnResponse> = vec![
            serde_json::from_value(serde_json::json!({
                "id":"q0","turnNumber":0,
                "rootInfo":{"winrate":0.60,"scoreLead":2.0,"currentPlayer":"B","visits":100},
                "moveInfos":[{"move":"D4","winrate":0.60,"scoreLead":2.0,"prior":0.2,"visits":80,"order":0,"pv":["D4","Q16"]}]
            })).unwrap(),
            serde_json::from_value(serde_json::json!({
                "id":"q0","turnNumber":1,
                "rootInfo":{"winrate":0.55,"scoreLead":1.0,"currentPlayer":"W","visits":100},
                "moveInfos":[]
            })).unwrap(),
        ];

        let analysis = assemble(&game_one_move(), turns, 5);
        assert_eq!(analysis.moves.len(), 1);
        let mv = &analysis.moves[0];
        assert_eq!(mv.color, Color::Black);
        assert_eq!(mv.mv, "Q16");
        assert!((mv.best_winrate - 0.60).abs() < 1e-5);
        assert!((mv.winrate - 0.45).abs() < 1e-5);
        assert!((mv.winrate_loss - 0.15).abs() < 1e-5);
        assert_eq!(mv.winrate_classification, Classification::Blunder);
        assert!((mv.score_loss - 3.0).abs() < 1e-5);
        assert_eq!(mv.score_classification, Classification::Mistake);
        assert_eq!(analysis.summary.by_winrate.black_blunders, 1);
        assert_eq!(analysis.summary.by_score.black_mistakes, 1);
        assert_eq!(mv.top_moves.len(), 1);
        assert_eq!(mv.top_moves[0].mv, "D4");
    }
}
