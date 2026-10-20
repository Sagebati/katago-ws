//! Analysis engine: parse SGF → drive KataGo → annotate. The muxa plugin
//! spawns the engine and shares it as `Arc<AnalysisEngine>` on the app state.

pub mod annotate;
pub mod autotune;
pub mod human;
pub mod katago;
pub mod sgf;

use std::collections::BTreeMap;
use std::sync::Arc;

use muxa::Sections;
use muxa::prelude::*;
use tokio::sync::{mpsc, watch};
use tracing::Instrument as _;

use crate::config::{EngineConfig, WorkerConfig};
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

    /// [`spawn`](Self::spawn), also forwarding KataGo's stderr lines to `tap`.
    pub async fn spawn_with_stderr(
        cfg: EngineConfig,
        tap: Option<mpsc::Sender<String>>,
    ) -> AppResult<Self> {
        let katago = KataGo::spawn_with_stderr(&cfg, tap).await?;
        Ok(Self { katago, cfg })
    }

    /// [`spawn`](Self::spawn) for the long-lived service engine: KataGo is
    /// relaunched if it exits (see [`KataGo::spawn_supervised`]).
    pub async fn spawn_supervised(cfg: EngineConfig) -> AppResult<Self> {
        let katago = KataGo::spawn_supervised(&cfg).await?;
        Ok(Self { katago, cfg })
    }

    /// Wait until KataGo has loaded its model and accepts queries.
    pub async fn wait_ready(&self) -> AppResult<()> {
        self.katago.wait_ready().await
    }

    /// The configuration the engine runs with (after auto-tune).
    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// Running count of positions KataGo has analyzed.
    #[must_use]
    pub fn positions(&self) -> watch::Receiver<u64> {
        self.katago.positions()
    }

    /// Kill KataGo and wait for it to exit.
    pub async fn shutdown(&self) {
        self.katago.shutdown().await;
    }

    /// Analyze `turns` of an already-parsed game at `max_visits`, without
    /// annotating them; returns how many positions were analyzed. (The
    /// auto-tuner's workload.)
    pub async fn analyze_positions(
        &self,
        game: &ParsedGame,
        turns: std::ops::Range<usize>,
    ) -> AppResult<usize> {
        let turns: Vec<usize> = turns.collect();
        let done = self
            .katago
            .analyze(game, &self.cfg, turns, self.cfg.max_visits, 0, None)
            .await?;
        Ok(done.len())
    }

    /// Analyze a full game and annotate every move.
    ///
    /// One pass at `max_visits` over every turn, or — with
    /// [`EngineConfig::two_pass`] — a cheap screening pass over every turn and
    /// a full-depth pass over only the turns around moves that swing (see
    /// [`annotate::turns_to_refine`]).
    ///
    /// `human_profile` (`rank_5k`, …; see [`human`]) adds the human SL
    /// network's policy to the payload when this engine has a human model. On
    /// one without, the profile is dropped with a warning rather than failing
    /// the job: KataGo would reject every query that carried it.
    #[tracing::instrument(name = "katago.analyze", skip_all, fields(moves, refined))]
    pub async fn analyze(&self, sgf: &str, human_profile: Option<&str>) -> AppResult<GameAnalysis> {
        let game = sgf::parse(sgf, self.cfg.default_board_size, self.cfg.default_komi)?;
        let span = tracing::Span::current();
        span.record("moves", game.moves.len());
        let human_profile = human_profile.filter(|profile| {
            let loaded = !self.cfg.human_model.is_empty();
            if !loaded {
                tracing::warn!(
                    profile,
                    "job asked for a human profile but no [engine] human_model is loaded; \
                     analysing without human policy"
                );
            }
            loaded
        });
        let all_turns: Vec<usize> = (0..=game.moves.len()).collect();

        let turns = if self.cfg.two_pass() {
            let screened = self
                .pass(
                    Pass::Screen,
                    &game,
                    all_turns,
                    self.cfg.screen_visits,
                    human_profile,
                )
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
                    human_profile,
                )
                .await?;
            merge_turns(screened, refined)
        } else {
            self.pass(
                Pass::Full,
                &game,
                all_turns,
                self.cfg.max_visits,
                human_profile,
            )
            .await?
        };
        Ok(annotate::assemble(&game, turns, &self.cfg, human_profile))
    }

    /// One KataGo query over `turns`, traced and counted as `pass`.
    async fn pass(
        &self,
        pass: Pass,
        game: &ParsedGame,
        turns: Vec<usize>,
        visits: u32,
        human_profile: Option<&str>,
    ) -> AppResult<Vec<TurnResponse>> {
        let positions = turns.len();
        let span = tracing::info_span!("katago.pass", pass = pass.as_str(), positions, visits);
        let result = self
            .katago
            .analyze(
                game,
                &self.cfg,
                turns,
                visits,
                pass.priority(),
                human_profile,
            )
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

/// What the plugin reads: `[engine]`, plus what the auto-tune needs to know
/// about it — which tuning fields were set explicitly, and how many games a
/// worker keeps in flight (`[worker].concurrency`).
#[derive(Debug, Default, serde::Deserialize)]
pub struct EngineSetup {
    #[serde(skip)]
    engine: EngineConfig,
    #[serde(skip)]
    explicit: Vec<&'static str>,
    #[serde(skip)]
    concurrency: usize,
}

impl EngineSetup {
    /// Read `[engine]` and the auto-tune's context from the config sections.
    pub fn read(sections: &Sections<'_>) -> Result<Self> {
        let engine: EngineConfig = sections.get("engine")?;
        let concurrency = sections.get::<WorkerConfig>("worker")?.concurrency;
        // Tuning fields given explicitly (file or env) are pinned; defaults aren't.
        let mut explicit = Vec::new();
        for (field, _) in autotune::TUNING_FIELDS {
            if sections
                .get::<Option<u32>>(&format!("engine.{field}"))?
                .is_some()
            {
                explicit.push(field);
            }
        }
        Ok(Self {
            engine,
            explicit,
            concurrency,
        })
    }

    /// `[engine]` after the auto-tune (or as given, when it is off or fails).
    pub async fn tuned(self, force: bool) -> EngineConfig {
        autotune::apply(self.engine, self.concurrency, &self.explicit, force).await
    }
}

/// muxa plugin for `katago-ws autotune`: tune KataGo for this host into the
/// cache and log the result, without keeping an engine running. Runs even
/// when `[engine] autotune = false` — asking for it is explicit.
pub struct AutotunePlugin {
    /// Use the fast search regardless of `[engine] autotune_mode`.
    pub fast: bool,
    /// Re-tune even when a cached result exists.
    pub force: bool,
}

impl<S: State> Plugin<S> for AutotunePlugin {
    type Output = ();
    type Config = EngineSetup;
    const CONFIG_PREFIX: &'static str = "engine";

    fn read_config(sections: &Sections<'_>) -> Result<EngineSetup> {
        EngineSetup::read(sections)
    }

    async fn build(self, mut setup: EngineSetup, _state: &S, _ctx: &mut BuildCtx) -> Result<()> {
        setup.engine.autotune = true;
        if self.fast {
            setup.engine.autotune_mode = crate::config::AutotuneMode::Fast;
        }
        let cfg = setup.tuned(self.force).await;
        tracing::info!(
            overrides = cfg.katago_overrides().as_deref().unwrap_or("none"),
            "autotune: KataGo will start with"
        );
        Ok(())
    }
}

impl<S: State> Plugin<S> for KataGoEnginePlugin {
    type Output = Arc<AnalysisEngine>;
    type Config = EngineSetup;
    const CONFIG_PREFIX: &'static str = "engine";

    fn read_config(sections: &Sections<'_>) -> Result<EngineSetup> {
        EngineSetup::read(sections)
    }

    async fn build(
        self,
        setup: EngineSetup,
        _state: &S,
        _ctx: &mut BuildCtx,
    ) -> Result<Arc<AnalysisEngine>> {
        let cfg = setup.tuned(false).await;
        tracing::info!(
            binary = %cfg.binary,
            model = %cfg.model,
            max_visits = cfg.max_visits,
            human_model = (!cfg.human_model.is_empty()).then_some(cfg.human_model.as_str()),
            screen_visits = cfg.two_pass().then_some(cfg.screen_visits),
            overrides = cfg.katago_overrides().as_deref().unwrap_or("none"),
            "launching KataGo analysis engine"
        );
        let engine = AnalysisEngine::spawn_supervised(cfg)
            .await
            .map_err(Error::other)?;
        // Fail startup here, with KataGo's own reason, rather than come up
        // "ready" and fail every job.
        engine.wait_ready().await.map_err(Error::other)?;
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
