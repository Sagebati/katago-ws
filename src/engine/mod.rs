//! Analysis engine: parse SGF → drive KataGo → annotate. The muxa plugin
//! spawns the engine and shares it as `Arc<AnalysisEngine>` on the app state.

pub mod annotate;
pub mod katago;
pub mod sgf;

use std::collections::BTreeMap;
use std::sync::Arc;

use muxa::prelude::*;
use tracing::Instrument as _;

use crate::config::EngineConfig;
use crate::engine::annotate::GameAnalysis;
use crate::engine::katago::{KataGo, TurnResponse};
use crate::engine::sgf::ParsedGame;
use crate::error::AppResult;

/// Owns the KataGo client and turns an SGF into a [`GameAnalysis`].
pub struct AnalysisEngine {
    katago: Arc<KataGo>,
    cfg: EngineConfig,
}

impl AnalysisEngine {
    /// Spawn the KataGo analysis engine subprocess.
    pub async fn spawn(cfg: EngineConfig) -> AppResult<Self> {
        let katago = KataGo::spawn(&cfg).await?;
        Ok(Self { katago, cfg })
    }

    /// Analyze a full game and annotate every move.
    ///
    /// One pass at `max_visits` over every turn, or — with
    /// [`EngineConfig::two_pass`] — a cheap screening pass over every turn and
    /// a full-depth pass over only the turns around moves that swing (see
    /// [`annotate::turns_to_refine`]).
    #[tracing::instrument(name = "katago.analyze", skip_all, fields(moves, refined))]
    pub async fn analyze(&self, sgf: &str) -> AppResult<GameAnalysis> {
        let game = sgf::parse(sgf, self.cfg.default_board_size, self.cfg.default_komi)?;
        let span = tracing::Span::current();
        span.record("moves", game.moves.len());
        let all_turns: Vec<usize> = (0..=game.moves.len()).collect();

        let turns = if self.cfg.two_pass() {
            let screened = self
                .pass(Pass::Screen, &game, all_turns, self.cfg.screen_visits)
                .await?;
            let refine = annotate::turns_to_refine(&game, &screened, &self.cfg);
            span.record("refined", refine.len());
            // Refinement goes ahead of other games' screening in KataGo's
            // queue, so a game isn't held back waiting for its few deep
            // searches behind everyone else's shallow ones.
            let refined = self
                .pass(
                    Pass::Refine,
                    &game,
                    refine.into_iter().collect(),
                    self.cfg.max_visits,
                )
                .await?;
            merge_turns(screened, refined)
        } else {
            self.pass(Pass::Full, &game, all_turns, self.cfg.max_visits)
                .await?
        };
        Ok(annotate::assemble(&game, turns, &self.cfg))
    }

    /// One KataGo query over `turns`, traced and counted as `pass`.
    async fn pass(
        &self,
        pass: Pass,
        game: &ParsedGame,
        turns: Vec<usize>,
        visits: u32,
    ) -> AppResult<Vec<TurnResponse>> {
        let positions = turns.len();
        let span = tracing::info_span!("katago.pass", pass = pass.as_str(), positions, visits);
        let result = self
            .katago
            .analyze(game, &self.cfg, turns, visits, pass.priority())
            .instrument(span)
            .await?;
        crate::metrics::metrics().positions_analyzed(pass.as_str(), positions as u64);
        Ok(result)
    }
}

/// Which search a KataGo query is.
#[derive(Debug, Clone, Copy)]
enum Pass {
    /// Single-pass analysis: every turn at `max_visits`.
    Full,
    /// Two-pass, first pass: every turn at `screen_visits`.
    Screen,
    /// Two-pass, second pass: the swinging turns at `max_visits`.
    Refine,
}

impl Pass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Screen => "screen",
            Self::Refine => "refine",
        }
    }

    /// KataGo queue priority: refinement finishes games already in flight.
    fn priority(self) -> i32 {
        match self {
            Self::Full | Self::Screen => 0,
            Self::Refine => 1,
        }
    }
}

/// Overlay the refined turns on the screened ones, by turn number.
fn merge_turns(screened: Vec<TurnResponse>, refined: Vec<TurnResponse>) -> Vec<TurnResponse> {
    let mut by_turn: BTreeMap<usize, TurnResponse> = screened
        .into_iter()
        .map(|turn| (turn.turn_number, turn))
        .collect();
    by_turn.extend(refined.into_iter().map(|turn| (turn.turn_number, turn)));
    by_turn.into_values().collect()
}

/// muxa plugin: launches KataGo and shares the engine on the state HList.
#[derive(Default)]
pub struct KataGoEnginePlugin;

impl<S: State> Plugin<S> for KataGoEnginePlugin {
    type Output = Arc<AnalysisEngine>;
    type Config = EngineConfig;
    const CONFIG_PREFIX: &'static str = "engine";

    async fn build(
        self,
        cfg: EngineConfig,
        _state: &S,
        _ctx: &mut BuildCtx,
    ) -> Result<Arc<AnalysisEngine>> {
        tracing::info!(
            binary = %cfg.binary,
            model = %cfg.model,
            max_visits = cfg.max_visits,
            screen_visits = cfg.two_pass().then_some(cfg.screen_visits),
            overrides = cfg.katago_overrides().as_deref().unwrap_or("none"),
            "launching KataGo analysis engine"
        );
        let engine = AnalysisEngine::spawn(cfg).await.map_err(Error::other)?;
        tracing::info!("KataGo analysis engine ready");
        Ok(Arc::new(engine))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(number: usize, visits: u32) -> TurnResponse {
        serde_json::from_value(serde_json::json!({
            "id": "q0", "turnNumber": number,
            "rootInfo": {"winrate": 0.5, "scoreLead": 0.0, "visits": visits},
            "moveInfos": [{"move": "D4", "winrate": 0.5, "visits": visits}]
        }))
        .unwrap()
    }

    #[test]
    fn refined_turns_replace_their_screened_versions_in_turn_order() {
        let screened = vec![turn(2, 16), turn(0, 16), turn(1, 16), turn(3, 16)];
        let refined = vec![turn(2, 400), turn(1, 400)];
        let merged = merge_turns(screened, refined);
        let shape: Vec<_> = merged
            .iter()
            .map(|turn| (turn.turn_number, turn.move_infos[0].visits))
            .collect();
        assert_eq!(shape, vec![(0, 16), (1, 400), (2, 400), (3, 16)]);
    }
}
