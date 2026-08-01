//! Turn KataGo per-turn analysis into the annotation payload returned to clients.
//!
//! Loss for move *i* is the difference between two consecutive root
//! evaluations: what the position was worth to the mover before they played,
//! minus what it was worth to them after. Differencing consecutive positions
//! rather than comparing against a candidate list stays correct even when the
//! move played isn't among KataGo's reported candidates.
//!
//! The subtlety that makes this hard — and that this module got wrong until
//! `payload_version = 2` — is that the two evaluations must be *in the same
//! player's frame*. Turns `i` and `i+1` belong to opposite players, and KataGo
//! reports in whichever fixed frame `reportAnalysisWinratesAs` names, so
//! "rotate both into the mover's frame" and "negate the second one" are
//! different operations that coincide only when the engine reports side-to-move.
//! See [`crate::engine::katago::Perspective`].

use std::collections::HashMap;

use serde::Serialize;

use crate::config::EngineConfig;
use crate::engine::katago::{REPORT_PERSPECTIVE, TurnResponse};
use crate::engine::sgf::{Color, ParsedGame, Rules};

/// The payload contract version.
///
/// Bumped when a field's *meaning* changes, not when one is added — every
/// consumer tolerates unknown fields.
///
/// - `1` — assumed KataGo reported from the side to move. Every per-move loss
///   in a v1 payload is the wrong number, not merely an imprecise one.
/// - `2` — perspective-correct, and carries per-turn ownership.
pub const PAYLOAD_VERSION: u32 = 2;

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

/// What produced a payload.
///
/// Recorded so a metric from one engine build is never silently compared with
/// another's: the same game at 40 visits and at 500 gives different point
/// losses, and neither is wrong.
#[derive(Serialize)]
pub struct EngineInfo {
    /// Engine name — always `katago`.
    pub name: &'static str,
    /// Neural net the analysis ran against.
    pub model: String,
    /// Visits spent per analysed position.
    pub max_visits: u32,
    /// Candidates reported per position.
    pub top_k: usize,
    /// The frame KataGo was told to report in, before normalisation. Stored so
    /// a future perspective bug is diagnosable from a payload alone.
    pub report_perspective: &'static str,
    /// Whether ownership maps were requested.
    pub ownership: bool,
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
    /// Which payload contract this was built under. See [`PAYLOAD_VERSION`].
    pub payload_version: u32,
    /// What produced it.
    pub engine: EngineInfo,
    /// Expected ownership of every intersection at each turn `0..=moves.len()`,
    /// row-major from the top-left, in hundredths and **always Black-positive**
    /// whatever frame the engine reported in.
    ///
    /// Indexed by *turn*, not by move: turn `i` is the position before move
    /// `i + 1`, so turn `i + 1` is that move's "after". Storing before and
    /// after per move would duplicate every array for nothing.
    ///
    /// Empty when the worker ran without `includeOwnership`, and — deliberately
    /// — all-or-nothing: a partial set would invite consumers to index into it
    /// and land on the wrong turn.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ownership: Vec<Vec<i8>>,
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

/// Who is to move at `turn`.
///
/// Turn `i` is the position *before* move `i`, so the side to move is simply
/// the colour of move `i`. The one turn past the end of the move list belongs
/// to whoever would have played next. Derived from the move list rather than
/// assumed to alternate, because handicap games start with White and a game
/// record can contain consecutive moves by the same colour.
fn side_to_move_at(game: &ParsedGame, turn: usize) -> Color {
    game.moves.get(turn).map_or_else(
        || {
            game.moves
                .last()
                .map_or(Color::Black, |(color, _)| color.other())
        },
        |(color, _)| *color,
    )
}

/// Warn once if KataGo's `currentPlayer` disagrees with the move list.
///
/// A mismatch means the turn indices are skewed, which would attribute every
/// loss to the wrong player — silently, and with numbers that still look
/// plausible. Worth saying out loud rather than quietly correcting, because the
/// correction would depend on guessing the skew.
fn check_turn_alignment(game: &ParsedGame, by_turn: &HashMap<usize, TurnResponse>) {
    let mismatched = (0..=game.moves.len())
        .filter(|&turn| {
            by_turn
                .get(&turn)
                .and_then(|response| response.root_info.as_ref())
                .and_then(|root| root.current_player)
                .is_some_and(|reported| reported != side_to_move_at(game, turn))
        })
        .count();

    if mismatched > 0 {
        tracing::warn!(
            mismatched,
            turns = game.moves.len() + 1,
            "KataGo's currentPlayer disagrees with the move list — per-move \
             losses may be attributed to the wrong player"
        );
    }
}

/// Ownership at every turn, normalised to Black-positive hundredths.
///
/// Returns empty unless *every* turn carries a correctly sized map: consumers
/// index this by turn number, so a gap would silently shift a swing onto the
/// neighbouring move. All-or-nothing is the only safe contract.
fn normalise_ownership(game: &ParsedGame, by_turn: &HashMap<usize, TurnResponse>) -> Vec<Vec<i8>> {
    let expected = usize::from(game.board_size) * usize::from(game.board_size);
    let mut per_turn = Vec::with_capacity(game.moves.len() + 1);

    for turn in 0..=game.moves.len() {
        let Some(raw) = by_turn
            .get(&turn)
            .and_then(|response| response.ownership.as_ref())
        else {
            return Vec::new();
        };
        if raw.len() != expected {
            tracing::warn!(
                turn,
                got = raw.len(),
                expected,
                "ownership map has the wrong length — dropping ownership for this game"
            );
            return Vec::new();
        }

        // The map is in the reported frame; flip it when that frame isn't
        // Black. An ownership array carries no `currentPlayer` of its own, so
        // the side to move comes from the move list.
        let reporter = REPORT_PERSPECTIVE.reporter(None, side_to_move_at(game, turn));
        let flip = reporter != Color::Black;

        per_turn.push(
            raw.iter()
                .map(|&cell| {
                    let signed = if flip { -cell } else { cell };
                    // Hundredths keep every digit a classifier can use while
                    // making the array an order of magnitude smaller than f32s.
                    (signed * 100.0).round().clamp(-100.0, 100.0) as i8
                })
                .collect(),
        );
    }

    per_turn
}

/// Assemble the analysis from the parsed game and KataGo's per-turn results.
pub fn assemble(game: &ParsedGame, turns: Vec<TurnResponse>, cfg: &EngineConfig) -> GameAnalysis {
    let by_turn: HashMap<usize, TurnResponse> = turns
        .into_iter()
        .map(|turn| (turn.turn_number, turn))
        .collect();

    check_turn_alignment(game, &by_turn);

    let mut moves = Vec::with_capacity(game.moves.len());
    let mut black = PlayerAccum::default();
    let mut white = PlayerAccum::default();

    for (index, (color, mv)) in game.moves.iter().enumerate() {
        // Both evaluations rotated into the mover's frame. Turn `index` is
        // their own turn; turn `index + 1` is their opponent's — which is why
        // reading the second one as-is (or merely negating it) is wrong unless
        // the engine happens to report side-to-move.
        let before = by_turn
            .get(&index)
            .and_then(|turn| turn.root_info.as_ref())
            .map(|root| root.as_seen_by(*color, *color));
        let after = by_turn
            .get(&(index + 1))
            .and_then(|turn| turn.root_info.as_ref())
            .map(|root| root.as_seen_by(*color, color.other()));

        let best_winrate = before.map_or(0.5, |eval| eval.winrate);
        let winrate = after.map_or(best_winrate, |eval| eval.winrate);
        let winrate_loss = (best_winrate - winrate).max(0.0);
        let winrate_classification = classify_winrate(winrate_loss);

        let best_score = before.map_or(0.0, |eval| eval.score_lead);
        let resulting_score = after.map_or(best_score, |eval| eval.score_lead);
        let score_loss = (best_score - resulting_score).max(0.0);
        let score_classification = classify_score(score_loss);

        let top_moves = by_turn
            .get(&index)
            .map(|turn| {
                let mut infos: Vec<&_> = turn.move_infos.iter().collect();
                infos.sort_by_key(|info| info.order);
                infos
                    .into_iter()
                    .take(cfg.top_k)
                    .map(|info| {
                        // Candidates at this turn belong to the mover.
                        let eval = info.as_seen_by(*color, *color);
                        Candidate {
                            mv: info.mv.clone(),
                            winrate: eval.winrate,
                            score_lead: eval.score_lead,
                            prior: info.prior,
                            visits: info.visits,
                            pv: info.pv.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let accum = match color {
            Color::Black => &mut black,
            Color::White => &mut white,
        };
        accum.record(
            winrate_loss,
            score_loss,
            winrate_classification,
            score_classification,
        );

        moves.push(MoveAnnotation {
            move_number: index + 1,
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

    let ownership = normalise_ownership(game, &by_turn);

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
        payload_version: PAYLOAD_VERSION,
        engine: EngineInfo {
            name: "katago",
            model: cfg.model.clone(),
            max_visits: cfg.max_visits,
            top_k: cfg.top_k,
            report_perspective: REPORT_PERSPECTIVE.as_katago_setting(),
            ownership: !ownership.is_empty(),
        },
        ownership,
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

    /// A two-move game on a 2×2 board — small enough that an ownership map is
    /// four readable numbers.
    fn tiny_game() -> ParsedGame {
        ParsedGame {
            board_size: 2,
            komi: 0.5,
            rules: Rules::Japanese,
            result: None,
            initial_stones: vec![],
            moves: vec![
                (Color::Black, "A2".to_owned()),
                (Color::White, "B1".to_owned()),
            ],
        }
    }

    fn cfg(top_k: usize) -> EngineConfig {
        EngineConfig {
            top_k,
            max_visits: 100,
            model: "test-model".to_owned(),
            ..EngineConfig::default()
        }
    }

    /// Turn responses for [`tiny_game`], in side-to-move perspective, with an
    /// ownership map per turn. `owner` values are in `[-1, 1]`.
    fn tiny_turns(ownership: [Option<Vec<f32>>; 3]) -> Vec<TurnResponse> {
        let players = ["B", "W", "B"];
        ownership
            .into_iter()
            .enumerate()
            .map(|(turn, owner)| {
                serde_json::from_value(serde_json::json!({
                    "id": "q0",
                    "turnNumber": turn,
                    "rootInfo": {
                        "winrate": 0.5, "scoreLead": 0.0,
                        "currentPlayer": players[turn], "visits": 10
                    },
                    "moveInfos": [],
                    "ownership": owner,
                }))
                .expect("fixture matches the response types")
            })
            .collect()
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

        let analysis = assemble(&game_one_move(), turns, &cfg(5));
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

    /// The 9×9 symptom from the README: `best_winrate < winrate` on every move,
    /// so the loss clamps to zero and a whole game scores as flawless. Whatever
    /// else changes, a mover's own best line can never be worse than what they
    /// actually got.
    #[test]
    fn a_move_never_beats_the_best_line_available_to_it() {
        let turns = tiny_turns([None, None, None]);
        let analysis = assemble(&tiny_game(), turns, &cfg(5));

        for mv in &analysis.moves {
            assert!(
                mv.best_winrate >= mv.winrate,
                "move {} claims a better result than the best line",
                mv.move_number
            );
            assert!(mv.winrate_loss >= 0.0 && mv.score_loss >= 0.0);
        }
    }

    /// The 19×19 symptom: `score_loss ≈ 2 × score_lead`, rising with the lead,
    /// because the two evaluations were differenced across opposite frames. A
    /// commanding, *stable* lead must produce no loss at all.
    #[test]
    fn a_steady_large_lead_is_not_read_as_a_huge_loss() {
        // Black is +12 before and after their move — they gave up nothing.
        let turns: Vec<TurnResponse> = vec![
            serde_json::from_value(serde_json::json!({
                "id":"q0","turnNumber":0,
                "rootInfo":{"winrate":0.95,"scoreLead":12.0,"currentPlayer":"B","visits":100},
                "moveInfos":[]
            }))
            .unwrap(),
            // Same position from White's side: they are 12 points down.
            serde_json::from_value(serde_json::json!({
                "id":"q0","turnNumber":1,
                "rootInfo":{"winrate":0.05,"scoreLead":-12.0,"currentPlayer":"W","visits":100},
                "moveInfos":[]
            }))
            .unwrap(),
        ];

        let analysis = assemble(&game_one_move(), turns, &cfg(5));
        let mv = &analysis.moves[0];

        assert!(
            mv.score_loss.abs() < 1e-5,
            "a steady +12 lead was read as a {}-point loss",
            mv.score_loss
        );
        assert!(mv.winrate_loss.abs() < 1e-5);
        assert_eq!(mv.score_classification, Classification::Good);
    }

    /// Ownership is reported in the engine's frame and stored Black-positive,
    /// so a map taken on White's turn comes back negated.
    #[test]
    fn ownership_is_normalised_to_black_positive_hundredths() {
        let turns = tiny_turns([
            // Turn 0 — Black to move, so already Black-positive.
            Some(vec![1.0, 0.5, -0.5, -1.0]),
            // Turn 1 — White to move, so these are White's numbers.
            Some(vec![1.0, 0.5, -0.5, -1.0]),
            Some(vec![0.0, 0.0, 0.0, 0.0]),
        ]);
        let analysis = assemble(&tiny_game(), turns, &cfg(5));

        assert_eq!(analysis.ownership.len(), 3, "one map per turn");
        assert_eq!(analysis.ownership[0], vec![100, 50, -50, -100]);
        assert_eq!(
            analysis.ownership[1],
            vec![-100, -50, 50, 100],
            "White's frame is flipped into Black's"
        );
        assert!(analysis.engine.ownership);
        assert_eq!(analysis.payload_version, PAYLOAD_VERSION);
    }

    /// Consumers index ownership by turn number, so one missing or mis-sized
    /// map must drop the whole set rather than shift every later swing onto the
    /// wrong move.
    #[test]
    fn a_gap_or_a_wrong_length_drops_ownership_entirely() {
        let missing = tiny_turns([
            Some(vec![1.0, 0.0, 0.0, 0.0]),
            None,
            Some(vec![0.0, 0.0, 0.0, 0.0]),
        ]);
        let analysis = assemble(&tiny_game(), missing, &cfg(5));
        assert!(analysis.ownership.is_empty());
        assert!(!analysis.engine.ownership);

        let short = tiny_turns([
            Some(vec![1.0, 0.0, 0.0, 0.0]),
            Some(vec![1.0, 0.0]),
            Some(vec![0.0, 0.0, 0.0, 0.0]),
        ]);
        let analysis = assemble(&tiny_game(), short, &cfg(5));
        assert!(analysis.ownership.is_empty());
    }

    /// Turn `i` is the position before move `i`, and the turn past the end
    /// belongs to whoever would play next. Derived from the move list so a
    /// handicap game (White first) and a record with consecutive same-colour
    /// moves both come out right.
    #[test]
    fn the_side_to_move_follows_the_move_list_not_an_assumed_alternation() {
        let game = tiny_game();
        assert_eq!(side_to_move_at(&game, 0), Color::Black);
        assert_eq!(side_to_move_at(&game, 1), Color::White);
        assert_eq!(
            side_to_move_at(&game, 2),
            Color::Black,
            "past the last move"
        );

        let handicap = ParsedGame {
            moves: vec![
                (Color::White, "D4".to_owned()),
                (Color::White, "Q16".to_owned()),
            ],
            ..tiny_game()
        };
        assert_eq!(side_to_move_at(&handicap, 0), Color::White);
        assert_eq!(side_to_move_at(&handicap, 1), Color::White);
        assert_eq!(side_to_move_at(&handicap, 2), Color::Black);
    }

    /// Provenance travels with the payload so two analyses of the same game at
    /// different visit counts are never silently compared.
    #[test]
    fn the_payload_records_what_produced_it() {
        let analysis = assemble(&game_one_move(), tiny_turns([None, None, None]), &cfg(7));
        assert_eq!(analysis.engine.name, "katago");
        assert_eq!(analysis.engine.model, "test-model");
        assert_eq!(analysis.engine.max_visits, 100);
        assert_eq!(analysis.engine.top_k, 7);
        assert_eq!(analysis.engine.report_perspective, "SIDETOMOVE");
    }
}
