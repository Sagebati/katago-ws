//! Analysis engine: parse SGF → drive KataGo → annotate. The muxa plugin
//! spawns the engine and shares it as `Arc<AnalysisEngine>` on the app state.

pub mod annotate;
pub mod katago;
pub mod preflight;
pub mod sgf;
pub mod tune;

use std::sync::Arc;

use muxa::prelude::*;

use crate::config::EngineConfig;
use crate::engine::annotate::GameAnalysis;
use crate::engine::katago::KataGo;
use crate::error::AppResult;

/// Owns the KataGo client and turns an SGF into a [`GameAnalysis`].
pub struct AnalysisEngine {
    katago: Arc<KataGo>,
    cfg: EngineConfig,
}

impl AnalysisEngine {
    /// Preflight-check the binary/config/model, optionally auto-tune, then
    /// spawn the KataGo analysis engine subprocess.
    pub async fn spawn(cfg: EngineConfig) -> AppResult<Self> {
        let mut launch = preflight::check(&cfg)?;
        if cfg.auto_tune
            && let Some(tuning) = tune::resolve(&launch, &cfg).await
        {
            launch.overrides = tuning.overrides();
        }
        let katago = KataGo::spawn(&cfg, &launch).await?;
        Ok(Self { katago, cfg })
    }

    /// Analyze a full game and annotate every move.
    pub async fn analyze(&self, sgf: &str) -> AppResult<GameAnalysis> {
        let game = sgf::parse(sgf, self.cfg.default_board_size, self.cfg.default_komi)?;
        let turns = self.katago.analyze(&game, &self.cfg).await?;
        Ok(annotate::assemble(&game, turns, &self.cfg))
    }

    /// Whether the KataGo subprocess is still alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.katago.is_alive()
    }
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
            config = %cfg.config,
            model = %cfg.model,
            max_visits = cfg.max_visits,
            auto_tune = cfg.auto_tune,
            "launching KataGo analysis engine"
        );
        // `AnalysisEngine::spawn` (via `KataGo::spawn`) logs the actual
        // "ready" claim itself, once confirmed — nothing unconditional here.
        let engine = AnalysisEngine::spawn(cfg).await.map_err(Error::other)?;
        Ok(Arc::new(engine))
    }
}
