//! Analysis engine: parse SGF → drive KataGo → annotate. The muxa plugin
//! spawns the engine and shares it as `Arc<AnalysisEngine>` on the app state.

pub mod annotate;
pub mod katago;
pub mod sgf;

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
    /// Spawn the KataGo analysis engine subprocess.
    pub async fn spawn(cfg: EngineConfig) -> AppResult<Self> {
        let katago = KataGo::spawn(&cfg).await?;
        Ok(Self { katago, cfg })
    }

    /// Analyze a full game and annotate every move.
    pub async fn analyze(&self, sgf: &str) -> AppResult<GameAnalysis> {
        let game = sgf::parse(sgf, self.cfg.default_board_size, self.cfg.default_komi)?;
        let turns = self.katago.analyze(&game, &self.cfg).await?;
        Ok(annotate::assemble(&game, turns, self.cfg.top_k))
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
            model = %cfg.model,
            max_visits = cfg.max_visits,
            "launching KataGo analysis engine"
        );
        let engine = AnalysisEngine::spawn(cfg).await.map_err(Error::other)?;
        tracing::info!("KataGo analysis engine ready");
        Ok(Arc::new(engine))
    }
}
