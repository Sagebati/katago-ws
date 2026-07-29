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

/// Move-quality breakdown for one player under a single lens (win-rate loss or
/// score loss): how many of their moves fell into each [`Classification`].
#[derive(Serialize, Default)]
pub struct ClassCounts {
    /// Moves classified as `good`.
    pub good: usize,
    /// Moves classified as `inaccuracy`.
    pub inaccuracy: usize,
    /// Moves classified as `mistake`.
    pub mistake: usize,
    /// Moves classified as `blunder`.
    pub blunder: usize,
}

impl ClassCounts {
    /// Tally one move's classification.
    fn record(&mut self, classification: Classification) {
        match classification {
            Classification::Good => self.good += 1,
            Classification::Inaccuracy => self.inaccuracy += 1,
            Classification::Mistake => self.mistake += 1,
            Classification::Blunder => self.blunder += 1,
        }
    }
}

/// AI-review-style aggregate metrics for one player across the whole game.
#[derive(Serialize)]
pub struct PlayerReport {
    /// Number of moves this player made.
    pub moves: usize,
    /// Overall accuracy in `[0, 100]` — the mean of each move's accuracy, where a
    /// single move's accuracy is mapped from its win-rate loss by [`move_accuracy`].
    /// A flawless game approaches 100.
    pub accuracy: f32,
    /// Mean win probability given up per move (`[0, 1]`).
    pub mean_winrate_loss: f32,
    /// Mean points given up per move.
    pub mean_score_loss: f32,
    /// Move-quality breakdown by win-rate loss.
    pub by_winrate: ClassCounts,
    /// Move-quality breakdown by score loss.
    pub by_score: ClassCounts,
    /// Coarse strength estimate derived from `mean_score_loss`. Present only for
    /// full-size (19×19) games with enough moves to be meaningful; a rough
    /// indicator, not a calibrated rank. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_rank: Option<String>,
}

/// Whole-game report: per-player aggregate metrics (accuracy, mean loss, the
/// mistake/blunder breakdown, and a rough strength estimate).
#[derive(Serialize)]
pub struct GameReport {
    /// Number of annotated moves (both players).
    pub num_moves: usize,
    /// Black's aggregate metrics.
    pub black: PlayerReport,
    /// White's aggregate metrics.
    pub white: PlayerReport,
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
    /// Per-player aggregate report.
    pub report: GameReport,
}

/// Running per-player accumulator used while walking the move list; finalized
/// into a [`PlayerReport`] once every move is counted.
#[derive(Default)]
struct PlayerAccum {
    moves: usize,
    winrate_loss_sum: f32,
    score_loss_sum: f32,
    accuracy_sum: f32,
    by_winrate: ClassCounts,
    by_score: ClassCounts,
}

impl PlayerAccum {
    /// Fold one of the player's moves into the running totals.
    fn record(
        &mut self,
        winrate_loss: f32,
        score_loss: f32,
        winrate_class: Classification,
        score_class: Classification,
    ) {
        self.moves += 1;
        self.winrate_loss_sum += winrate_loss;
        self.score_loss_sum += score_loss;
        self.accuracy_sum += move_accuracy(winrate_loss);
        self.by_winrate.record(winrate_class);
        self.by_score.record(score_class);
    }

    /// Turn the accumulated totals into the player's report.
    fn finish(self, board_size: u8) -> PlayerReport {
        // Guard the empty-player case (e.g. a game with no White moves): means
        // are 0, not a division by zero.
        let denom = self.moves.max(1) as f32;
        let mean_score_loss = self.score_loss_sum / denom;
        PlayerReport {
            moves: self.moves,
            accuracy: self.accuracy_sum / denom,
            mean_winrate_loss: self.winrate_loss_sum / denom,
            mean_score_loss,
            by_winrate: self.by_winrate,
            by_score: self.by_score,
            estimated_rank: estimate_rank(board_size, self.moves, mean_score_loss),
        }
    }
}

/// Per-move accuracy in `[0, 100]` from win-rate loss, via the logistic mapping
/// popularised by Lichess: `103.1668·e^(−0.04354·Δ) − 3.1669`, where `Δ` is the
/// win probability dropped, in **percentage points**. A best move scores ~100 and
/// accuracy decays smoothly as a move gives up more win probability.
fn move_accuracy(winrate_loss: f32) -> f32 {
    let drop = (winrate_loss * 100.0).max(0.0);
    (103.1668 * (-0.04354 * drop).exp() - 3.1669).clamp(0.0, 100.0)
}

/// Below this move count a strength estimate is too noisy to report.
const RANK_MIN_MOVES: usize = 20;

/// Very rough strength band from mean points lost per move — monotone (fewer
/// points lost ⇒ stronger). Emitted only for full 19×19 games with at least
/// [`RANK_MIN_MOVES`] moves; it's a coarse indicator, not a calibrated rank, and
/// the thresholds are heuristic (tune to taste).
fn estimate_rank(board_size: u8, moves: usize, mean_score_loss: f32) -> Option<String> {
    if board_size != 19 || moves < RANK_MIN_MOVES {
        return None;
    }
    let band = if mean_score_loss < 0.8 {
        "~7d+"
    } else if mean_score_loss < 1.2 {
        "~5d"
    } else if mean_score_loss < 1.7 {
        "~3d"
    } else if mean_score_loss < 2.2 {
        "~1d"
    } else if mean_score_loss < 2.8 {
        "~2k"
    } else if mean_score_loss < 3.5 {
        "~5k"
    } else if mean_score_loss < 4.5 {
        "~8k"
    } else if mean_score_loss < 6.0 {
        "~12k"
    } else {
        "~15k+"
    };
    Some(band.to_owned())
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
    let mut black = PlayerAccum::default();
    let mut white = PlayerAccum::default();

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

        let accum = match color {
            Color::Black => &mut black,
            Color::White => &mut white,
        };
        accum.record(winrate_loss, score_loss, winrate_classification, score_classification);

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
        report: GameReport {
            num_moves: game.moves.len(),
            black: black.finish(game.board_size),
            white: white.finish(game.board_size),
        },
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
        // The move lands in Black's report under each lens.
        assert_eq!(analysis.report.num_moves, 1);
        assert_eq!(analysis.report.black.moves, 1);
        assert_eq!(analysis.report.white.moves, 0);
        assert_eq!(analysis.report.black.by_winrate.blunder, 1);
        assert_eq!(analysis.report.black.by_score.mistake, 1);
        assert_eq!(mv.top_moves.len(), 1);
        assert_eq!(mv.top_moves[0].mv, "D4");
    }

    #[test]
    fn move_accuracy_is_high_for_best_moves_and_low_for_blunders() {
        // No win-rate given up ⇒ ~100% accurate.
        assert!(move_accuracy(0.0) > 99.0);
        // A heavy 30-point win-prob drop ⇒ well below half.
        assert!(move_accuracy(0.30) < 30.0);
        // Monotone: giving up more is never more accurate.
        assert!(move_accuracy(0.05) > move_accuracy(0.20));
        // Always within bounds.
        assert!((0.0..=100.0).contains(&move_accuracy(1.0)));
    }

    #[test]
    fn rank_estimate_gated_by_board_size_and_move_count() {
        // Too few moves, or a non-19×19 board ⇒ no estimate.
        assert_eq!(estimate_rank(19, RANK_MIN_MOVES - 1, 1.0), None);
        assert_eq!(estimate_rank(9, 200, 1.0), None);
        // A strong, full-size game ⇒ a dan band; a sloppy one ⇒ a kyu band.
        assert_eq!(estimate_rank(19, 200, 0.9).as_deref(), Some("~5d"));
        assert_eq!(estimate_rank(19, 200, 5.0).as_deref(), Some("~12k"));
    }
}
