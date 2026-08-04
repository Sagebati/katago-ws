//! Client for KataGo's parallel analysis engine.
//!
//! Launches `katago analysis -config <cfg> -model <model>` once as a long-lived
//! subprocess and talks to it over newline-delimited JSON on stdin/stdout. The
//! protocol is asynchronous and id-keyed: one query (a whole game, all turns)
//! yields one response line per analyzed turn, possibly interleaved with other
//! in-flight queries.
//!
//! Concurrency is structured as an **actor**, not shared locks: a single owner
//! task exclusively holds stdin, the pending-request table, the id counter, and
//! the child handle — so no mutexes are needed. Callers submit a query over an
//! mpsc channel and await a oneshot reply; a reader task forwards stdout lines
//! to the same owner task, which demultiplexes them by `id`.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::EngineConfig;
use crate::engine::preflight::EngineLaunch;
use crate::engine::sgf::{Color, ColoredMove, ParsedGame, Rules};
use crate::error::{AppError, AppResult};

/// Buffer size of the request channel (callers → owner task). Caps how many
/// submitted-but-unstarted queries queue up before `analyze` callers await.
const CMD_QUEUE_CAPACITY: usize = 64;

/// Buffer size of the line channel (stdout reader → owner task). A whole-game
/// query emits one response line per turn (hundreds), so this absorbs a burst
/// without back-pressuring the reader.
const LINE_QUEUE_CAPACITY: usize = 1024;

/// Whose point of view KataGo's numbers are in.
///
/// The single most consequential setting in this pipeline, and not what you'd
/// assume. KataGo's docs: *"All values will be from the perspective of
/// `reportAnalysisWinratesAs` as specified in the analysis config file"* — and
/// the stock `analysis_example.cfg` this image ships sets it to `BLACK`.
///
/// Reading a fixed-perspective evaluation as though it alternated with the side
/// to move is what produced the two symptoms the tests below pin down: a
/// win-rate loss that clamps to zero on every move, and a score loss that comes
/// out at roughly twice the score lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the three variants model KataGo's three settings, not our choices — \
              only SIDETOMOVE is pinned today, but `reporter` handles all three so \
              that changing REPORT_PERSPECTIVE stays a one-line, correct edit"
)]
pub enum Perspective {
    /// Every value is Black's.
    Black,
    /// Every value is White's.
    White,
    /// Each value belongs to whoever is to move at that position.
    SideToMove,
}

impl Perspective {
    /// The `reportAnalysisWinratesAs` value that produces this perspective.
    #[must_use]
    pub fn as_katago_setting(self) -> &'static str {
        match self {
            Self::Black => "BLACK",
            Self::White => "WHITE",
            Self::SideToMove => "SIDETOMOVE",
        }
    }

    /// Whose evaluation a reading at this position is, given who is to move.
    ///
    /// `current_player` is KataGo's own claim about the position, preferred
    /// when present; pass `None` for a reading that carries none (an ownership
    /// map, say) to fall back on the caller's knowledge of the move list.
    #[must_use]
    pub fn reporter(self, current_player: Option<Color>, side_to_move: Color) -> Color {
        match self {
            Self::Black => Color::Black,
            Self::White => Color::White,
            // KataGo emits `currentPlayer`; the caller's value is only a
            // fallback for a build that stops doing so.
            Self::SideToMove => current_player.unwrap_or(side_to_move),
        }
    }
}

/// The perspective this service pins on every query, and therefore the frame
/// responses are interpreted in.
///
/// One constant drives both the `overrideSettings` we send and the rotation the
/// annotator applies, so the two cannot drift apart. That drift is precisely
/// how the original bug survived: the config file said one thing, the code
/// assumed another, and nothing connected them.
pub const REPORT_PERSPECTIVE: Perspective = Perspective::SideToMove;

/// A position evaluation in a known player's frame.
#[derive(Debug, Clone, Copy)]
pub struct Eval {
    /// Win probability for that player, in `[0, 1]`.
    pub winrate: f32,
    /// Expected score lead in points for that player.
    pub score_lead: f32,
}

impl Eval {
    /// The same evaluation seen from the other side. Win rate and score lead
    /// are both zero-sum.
    fn flipped(self) -> Self {
        Self {
            winrate: 1.0 - self.winrate,
            score_lead: -self.score_lead,
        }
    }
}

/// KataGo `rootInfo` for one analyzed turn (position evaluation).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInfo {
    /// Win probability in the [`Perspective`] the engine was told to report in
    /// — **not** necessarily the side to move. Use [`RootInfo::as_seen_by`]
    /// rather than reading this directly.
    pub winrate: f32,
    /// Expected score lead (points), in the same perspective as `winrate`.
    #[serde(default)]
    pub score_lead: f32,
    /// Who is to move at this position. Optional so an engine build that stops
    /// emitting it degrades to the caller's fallback rather than failing a job.
    #[serde(default)]
    pub current_player: Option<Color>,
}

impl RootInfo {
    /// This evaluation rotated into `viewer`'s frame.
    #[must_use]
    pub fn as_seen_by(&self, viewer: Color, side_to_move: Color) -> Eval {
        let eval = Eval {
            winrate: self.winrate,
            score_lead: self.score_lead,
        };
        if REPORT_PERSPECTIVE.reporter(self.current_player, side_to_move) == viewer {
            eval
        } else {
            eval.flipped()
        }
    }
}

/// KataGo `moveInfos` entry — one candidate move.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveInfo {
    /// Move in GTP notation. (Not camelCase — KataGo's key is literally `move`.)
    #[serde(rename = "move")]
    pub mv: String,
    /// Win probability after this move, in the reported [`Perspective`] — the
    /// same caveat as [`RootInfo::winrate`]. Use [`MoveInfo::as_seen_by`].
    pub winrate: f32,
    /// Score lead after this move, in the same perspective as `winrate`.
    #[serde(default)]
    pub score_lead: f32,
    /// Raw policy prior.
    #[serde(default)]
    pub prior: f32,
    /// Visits to this move.
    #[serde(default)]
    pub visits: u32,
    /// Rank among candidates (0 = best).
    #[serde(default)]
    pub order: u32,
    /// Principal variation following this move.
    #[serde(default)]
    pub pv: Vec<String>,
}

impl MoveInfo {
    /// This candidate's evaluation rotated into `viewer`'s frame.
    ///
    /// Candidates for turn `i` belong to the player to move at turn `i`, which
    /// under a fixed-perspective report is not who the numbers are stated for.
    /// Rotating them matters beyond cosmetics: a consumer comparing a user's
    /// move against these candidates would otherwise rank them backwards.
    #[must_use]
    pub fn as_seen_by(&self, viewer: Color, side_to_move: Color) -> Eval {
        let eval = Eval {
            winrate: self.winrate,
            score_lead: self.score_lead,
        };
        if REPORT_PERSPECTIVE.reporter(Some(side_to_move), side_to_move) == viewer {
            eval
        } else {
            eval.flipped()
        }
    }
}

/// One analyzed turn of a game.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnResponse {
    /// Which turn this is (0 = before the first move).
    #[serde(default)]
    pub turn_number: usize,
    /// Position evaluation. (`Option` → absent `rootInfo` deserializes to `None`.)
    pub root_info: Option<RootInfo>,
    /// Candidate moves, best first.
    #[serde(default)]
    pub move_infos: Vec<MoveInfo>,
    /// Expected ownership of every intersection, present only when
    /// `includeOwnership` is on. Length `boardYSize * boardXSize`, row-major
    /// from the top-left (A19 → T1), values in `[-1, 1]` in the reported
    /// [`Perspective`] — so it needs the same rotation as everything else.
    #[serde(default)]
    pub ownership: Option<Vec<f32>>,
}

/// One in-flight query: how many turn responses to expect, what's collected so
/// far, and where to deliver the completed batch.
struct Pending {
    expected: usize,
    got: Vec<TurnResponse>,
    tx: oneshot::Sender<Result<Vec<TurnResponse>, String>>,
}

/// A KataGo analysis query — the request side of the JSON protocol, modelled as
/// a typed struct (like the [`TurnResponse`] response types) rather than an
/// ad-hoc `json!` blob, so field names and types are checked at compile time.
/// `id` is filled in by the owner task so the correlation counter stays local.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalysisQuery {
    id: String,
    rules: Rules,
    komi: f32,
    board_x_size: u8,
    board_y_size: u8,
    initial_stones: Vec<ColoredMove>,
    moves: Vec<ColoredMove>,
    analyze_turns: Vec<usize>,
    max_visits: u32,
    // camelCase would give `analysisPvLen`; KataGo wants `PV` upper-cased.
    #[serde(rename = "analysisPVLen")]
    analysis_pv_len: u32,
    include_ownership: bool,
    /// Per-query settings that override the analysis config file.
    override_settings: OverrideSettings,
}

/// Query-level overrides of `analysis.cfg`.
///
/// The perspective is pinned here rather than trusted from the config file: the
/// config this image ships says `BLACK`, the annotator wants a frame it chose,
/// and a query-level override is the one place the two cannot disagree.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OverrideSettings {
    report_analysis_winrates_as: &'static str,
}

impl Default for OverrideSettings {
    fn default() -> Self {
        Self {
            report_analysis_winrates_as: REPORT_PERSPECTIVE.as_katago_setting(),
        }
    }
}

/// A submitted analysis request: the typed query (its `id` assigned by the owner
/// task), the expected turn count, and the reply channel.
struct AnalyzeCmd {
    query: AnalysisQuery,
    expected: usize,
    tx: oneshot::Sender<Result<Vec<TurnResponse>, String>>,
}

/// Handle to a running KataGo analysis engine. Cheap to share (`Arc<Self>` over
/// an mpsc sender); submit work with [`analyze`](Self::analyze).
pub struct KataGo {
    cmd_tx: mpsc::Sender<AnalyzeCmd>,
    timeout: Duration,
}

impl KataGo {
    /// Spawn the analysis engine against a preflight-validated [`EngineLaunch`].
    /// Waits (up to `cfg.startup_timeout_secs`) for KataGo's own ready signal
    /// so a bad model/config/GPU-driver crash is caught here, at startup, in
    /// place of the old "silent success, fails on the first job" behavior.
    /// Expiry of that wait is **not** treated as failure — only a real process
    /// exit during the window is — since a fresh GPU host's first-run
    /// CUDA/OpenCL autotune can take several minutes.
    pub async fn spawn(cfg: &EngineConfig, launch: &EngineLaunch) -> AppResult<Arc<Self>> {
        let mut command = Command::new(&launch.binary);
        command
            .arg("analysis")
            .arg("-config")
            .arg(&launch.config)
            .arg("-model")
            .arg(&launch.model);
        if !launch.overrides.is_empty() {
            let joined = launch
                .overrides
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",");
            command.arg("-override-config").arg(joined);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| spawn_error(&launch.binary, &err))?;

        tracing::info!(
            binary = %launch.binary.display(),
            config = %launch.config.display(),
            model = %launch.model.display(),
            "launched katago analysis"
        );

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Bounded channels: callers → owner (queries), reader → owner (lines).
        let (cmd_tx, cmd_rx) = mpsc::channel::<AnalyzeCmd>(CMD_QUEUE_CAPACITY);
        let (line_tx, line_rx) = mpsc::channel::<String>(LINE_QUEUE_CAPACITY);

        // Reader: forward each stdout line to the owner; ends on EOF, and
        // dropping `line_tx` signals "process gone" to the owner.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line_tx.send(line).await.is_err() {
                    break;
                }
            }
        });

        // Stderr: teed to the startup handshake below (logged at `info!`)
        // while it's listening; once that ends, falls back to `debug!` — a
        // single owner task decides which, so no shared state is needed.
        let (startup_tx, mut startup_rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut tee = Some(startup_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                let forwarded = match &tee {
                    Some(tx) if tx.send(line.clone()).await.is_ok() => true,
                    Some(_) => {
                        tee = None;
                        false
                    }
                    None => false,
                };
                if !forwarded {
                    tracing::debug!(target: "katago", "{line}");
                }
            }
        });

        let startup_timeout = Duration::from_secs(cfg.startup_timeout_secs.max(1));
        let started = Instant::now();
        let mut ready = false;
        // `tee_open` only gates the `recv` branch below — it must NOT end the
        // loop by itself. Stderr EOF and the process actually exiting are two
        // separate events that race close together (the fake-katago
        // regression test below exercises exactly this): if EOF alone ended
        // the loop, a real startup crash could be missed whenever `recv`
        // happens to win that race, falling through to "success" instead of
        // reporting the crash. The loop only ever ends via an explicit
        // `break` (ready, or timeout) or `return Err` (a real exit) — never
        // because the tee closed.
        let mut tee_open = true;
        loop {
            tokio::select! {
                line = startup_rx.recv(), if tee_open => match line {
                    Some(line) => {
                        tracing::info!(target: "katago", "{line}");
                        if is_ready_line(&line) {
                            ready = true;
                            break;
                        }
                    }
                    None => tee_open = false,
                },
                status = child.wait() => {
                    let status = status.map_err(|err| {
                        AppError::EngineStartup(format!("failed to wait on katago process: {err}"))
                    })?;
                    return Err(AppError::EngineStartup(format!(
                        "KataGo exited during startup with {status} — the model ({}) or \
                         analysis config ({}) is probably wrong for this KataGo build. \
                         Check the log lines above for KataGo's own diagnostic output.",
                        launch.model.display(),
                        launch.config.display(),
                    )));
                }
                () = tokio::time::sleep(startup_timeout) => {
                    tracing::warn!(
                        elapsed_secs = started.elapsed().as_secs(),
                        "KataGo has not reported ready yet; continuing anyway — first-run \
                         GPU tuning can take several minutes"
                    );
                    break;
                }
            }
        }
        if ready {
            tracing::info!(
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "KataGo analysis engine ready"
            );
        }
        drop(startup_rx);

        // Owner: the sole holder of stdin, the pending table, the id counter,
        // and the child — so the whole protocol runs without locks.
        tokio::spawn(owner_loop(child, stdin, cmd_rx, line_rx));

        Ok(Arc::new(Self {
            cmd_tx,
            timeout: Duration::from_secs(cfg.request_timeout_secs.max(1)),
        }))
    }

    /// Whether the owner task is still running (i.e. KataGo hasn't died).
    /// Free: the owner task drops its receiver on exit, which flips this.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// Analyze a whole game: returns one [`TurnResponse`] per turn `0..=moves`.
    pub async fn analyze(
        &self,
        game: &ParsedGame,
        cfg: &EngineConfig,
    ) -> AppResult<Vec<TurnResponse>> {
        let expected = game.moves.len() + 1; // turns 0..=moves.len()

        // `id` is assigned by the owner task (keeps the correlation counter local).
        let query = AnalysisQuery {
            id: String::new(),
            rules: game.rules,
            komi: game.komi,
            board_x_size: game.board_size,
            board_y_size: game.board_size,
            initial_stones: game.initial_stones.clone(),
            moves: game.moves.clone(),
            analyze_turns: (0..expected).collect(),
            max_visits: cfg.max_visits,
            analysis_pv_len: cfg.analysis_pv_len,
            include_ownership: cfg.include_ownership,
            override_settings: OverrideSettings::default(),
        };

        let (tx, rx) = oneshot::channel();
        #[allow(
            clippy::map_err_ignore,
            reason = "SendError only wraps the un-sent command, which carries no useful context"
        )]
        self.cmd_tx
            .send(AnalyzeCmd {
                query,
                expected,
                tx,
            })
            .await
            .map_err(|_| AppError::Inference("katago engine stopped".into()))?;

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(turns))) => Ok(turns),
            Ok(Ok(Err(err))) => Err(AppError::Inference(format!("katago: {err}"))),
            Ok(Err(_)) => Err(AppError::Inference(
                "katago engine dropped the request".into(),
            )),
            Err(_) => Err(AppError::Inference("katago analysis timed out".into())),
        }
    }
}

/// Wrap a `Command::spawn()` failure with an actionable hint. Preflight
/// (`engine::preflight::check`) already validates the binary before this is
/// ever reached, so this is a residual net for a TOCTOU race, a wrong-arch
/// binary (`ENOEXEC`), or a KataGo AppImage with no FUSE available.
fn spawn_error(binary: &std::path::Path, err: &std::io::Error) -> AppError {
    let hint = match err.kind() {
        std::io::ErrorKind::NotFound => {
            "it passed preflight but is gone now — did something remove it?"
        }
        std::io::ErrorKind::PermissionDenied => "check it's executable: chmod +x <path>",
        _ => {
            "if this is a KataGo AppImage release and the host has no FUSE, try: \
             APPIMAGE_EXTRACT_AND_RUN=1"
        }
    };
    AppError::EngineStartup(format!(
        "failed to launch '{} analysis': {err} ({hint})",
        binary.display()
    ))
}

/// Best-effort detection of KataGo's own "ready" line on stderr. If wording
/// ever drifts in a future KataGo release and this stops matching, startup
/// just falls through to the timeout path — degraded, not broken.
fn is_ready_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("ready to begin handling requests") || lower.contains("started, ready")
}

/// The single owner of stdin and the pending table. Assigns ids, writes
/// queries, and resolves replies as turn responses arrive — all from one task,
/// so the state is plain (no `Mutex`/`Arc`/atomics).
///
/// `child` is held live (not just to keep the subprocess alive via
/// `kill_on_drop`) so a mid-life exit can report the real exit status instead
/// of a bare "process exited".
async fn owner_loop(
    mut child: Child,
    mut stdin: ChildStdin,
    mut cmd_rx: mpsc::Receiver<AnalyzeCmd>,
    mut line_rx: mpsc::Receiver<String>,
) {
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut next_id: u64 = 0;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(AnalyzeCmd { mut query, expected, tx }) => {
                    let id = format!("q{next_id}");
                    next_id += 1;
                    query.id = id.clone();

                    let mut line = match serde_json::to_string(&query) {
                        Ok(json) => json,
                        Err(err) => {
                            let _ = tx.send(Err(format!("serialize query: {err}")));
                            continue;
                        }
                    };
                    line.push('\n');

                    // Write + flush as one fallible step.
                    let write = async {
                        stdin.write_all(line.as_bytes()).await?;
                        stdin.flush().await
                    };
                    if let Err(err) = write.await {
                        let _ = tx.send(Err(format!("write to katago: {err}")));
                        continue;
                    }

                    pending.insert(id, Pending { expected, got: Vec::with_capacity(expected), tx });
                }
                // All handles dropped → shut down (drops `child` → kill).
                None => break,
            },
            line = line_rx.recv() => match line {
                Some(line) => handle_line(&mut pending, &line),
                None => {
                    // stdout closed → process gone; report the real exit status
                    // rather than a bare "process exited", and fail everything
                    // outstanding.
                    let status = match child.wait().await {
                        Ok(status) => status.to_string(),
                        Err(err) => format!("unknown ({err})"),
                    };
                    tracing::error!(status = %status, "KataGo process exited unexpectedly");
                    for (_, req) in pending.drain() {
                        let _ = req.tx.send(Err(format!("katago process exited: {status}")));
                    }
                    break;
                }
            },
        }
    }
}

/// Demux one response line into the pending table (synchronous — the owner task
/// has exclusive access, so no locking).
fn handle_line(pending: &mut HashMap<String, Pending>, line: &str) {
    let value: Value = match serde_json::from_str(line) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(target: "katago", "unparsable line: {err}");
            return;
        }
    };

    let id = value
        .get("id")
        .and_then(|val| val.as_str())
        .map(str::to_owned);

    if let Some(err) = value.get("error").and_then(|val| val.as_str()) {
        match id {
            Some(id) => {
                if let Some(req) = pending.remove(&id) {
                    let _ = req.tx.send(Err(err.to_owned()));
                }
            }
            None => tracing::error!(target: "katago", "engine error: {err}"),
        }
        return;
    }
    if let Some(warn) = value.get("warning").and_then(|val| val.as_str()) {
        tracing::warn!(target: "katago", "warning: {warn}");
        return;
    }

    let Some(id) = id else { return };
    let turn: TurnResponse = match serde_json::from_value(value) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(target: "katago", "unparsable response: {err}");
            return;
        }
    };

    if let Some(req) = pending.get_mut(&id) {
        req.got.push(turn);
        // Deliver the whole batch once the final turn has arrived.
        if req.got.len() >= req.expected
            && let Some(completed) = pending.remove(&id)
        {
            let _ = completed.tx.send(Ok(completed.got));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(winrate: f32, score_lead: f32, current_player: Option<Color>) -> RootInfo {
        RootInfo {
            winrate,
            score_lead,
            current_player,
        }
    }

    /// The test that would have caught the original bug.
    ///
    /// The annotator's arithmetic is only correct if KataGo actually reports in
    /// the frame the annotator assumes. Nothing in the config file guarantees
    /// that — the shipped `analysis_example.cfg` says `BLACK` — so the setting
    /// is pinned per query. If this assertion ever fails, every per-move loss
    /// this service reports is wrong.
    #[test]
    fn every_query_pins_the_reporting_perspective() {
        let query = AnalysisQuery {
            id: "q0".to_owned(),
            rules: Rules::Japanese,
            komi: 7.5,
            board_x_size: 19,
            board_y_size: 19,
            initial_stones: vec![],
            moves: vec![],
            analyze_turns: vec![0],
            max_visits: 100,
            analysis_pv_len: 10,
            include_ownership: true,
            override_settings: OverrideSettings::default(),
        };

        let json = serde_json::to_value(&query).expect("the query serialises");
        assert_eq!(
            json["overrideSettings"]["reportAnalysisWinratesAs"],
            "SIDETOMOVE"
        );
        // The pin and the rotation are driven by one constant; they cannot drift.
        assert_eq!(
            json["overrideSettings"]["reportAnalysisWinratesAs"],
            REPORT_PERSPECTIVE.as_katago_setting()
        );
    }

    /// Win rate and score lead are zero-sum, so the same position seen from the
    /// other side is `1 - winrate` and `-score_lead` — and seeing it from your
    /// own side must change nothing.
    #[test]
    fn an_evaluation_rotates_into_either_players_frame() {
        let info = root(0.70, 5.0, Some(Color::Black));

        let black = info.as_seen_by(Color::Black, Color::Black);
        assert!((black.winrate - 0.70).abs() < 1e-6);
        assert!((black.score_lead - 5.0).abs() < 1e-6);

        let white = info.as_seen_by(Color::White, Color::Black);
        assert!((white.winrate - 0.30).abs() < 1e-6);
        assert!((white.score_lead + 5.0).abs() < 1e-6);
    }

    /// `currentPlayer` is what makes the rotation independent of any assumption
    /// about alternation; the caller's value is used only when it is absent.
    #[test]
    fn the_engines_own_current_player_wins_over_the_callers_guess() {
        // The engine says White is to move; the caller wrongly guessed Black.
        let info = root(0.70, 5.0, Some(Color::White));
        let eval = info.as_seen_by(Color::White, Color::Black);
        assert!(
            (eval.winrate - 0.70).abs() < 1e-6,
            "the engine's claim should have identified White as the reporter"
        );

        // With no claim, the caller's knowledge of the move list is all there is.
        let blind = root(0.70, 5.0, None);
        let eval = blind.as_seen_by(Color::White, Color::White);
        assert!((eval.winrate - 0.70).abs() < 1e-6);
        let eval = blind.as_seen_by(Color::White, Color::Black);
        assert!((eval.winrate - 0.30).abs() < 1e-6);
    }

    /// Why the pin matters, shown in numbers.
    ///
    /// These are the README's 19×19 figures: Black leads by 12 points and gives
    /// up nothing. Read as Black-perspective data — which is what the engine
    /// sends by default — a blind `-score_lead` on the following turn turns a
    /// steady lead into a 24-point "loss", exactly the `2 × score_lead` pattern
    /// observed. Rotating with a known perspective gives zero.
    #[test]
    fn misreading_the_perspective_doubles_the_lead_into_a_loss() {
        let before = 12.0_f32;
        // Turn i+1 in BLACK perspective: still +12 for Black.
        let after_reported_as_black = 12.0_f32;

        let naive_loss = before - (-after_reported_as_black);
        assert!(
            (naive_loss - 24.0).abs() < 1e-6,
            "the old arithmetic is the README's 2 x score_lead symptom"
        );

        // The same reading, rotated knowing it is Black's: Black gave up nothing.
        let info = root(0.95, after_reported_as_black, Some(Color::Black));
        let correct = before - info.as_seen_by(Color::Black, Color::Black).score_lead;
        assert!((correct).abs() < 1e-6);
    }

    /// The rotation is correct under *any* of the three settings, not only the
    /// one pinned — so a deliberate change to [`REPORT_PERSPECTIVE`] stays
    /// correct, and the pin is a second line of defence rather than the only one.
    #[test]
    fn the_reporter_is_identified_under_every_setting() {
        // Black to move, and the engine says so.
        let claim = Some(Color::Black);
        assert_eq!(
            Perspective::Black.reporter(claim, Color::Black),
            Color::Black
        );
        assert_eq!(
            Perspective::White.reporter(claim, Color::Black),
            Color::White,
            "a WHITE-perspective report is White's whoever is to move"
        );
        assert_eq!(
            Perspective::SideToMove.reporter(claim, Color::Black),
            Color::Black
        );

        // With White to move, only SIDETOMOVE changes its answer.
        let claim = Some(Color::White);
        assert_eq!(
            Perspective::Black.reporter(claim, Color::White),
            Color::Black
        );
        assert_eq!(
            Perspective::SideToMove.reporter(claim, Color::White),
            Color::White
        );

        // Each maps to the config value that produces it.
        assert_eq!(Perspective::Black.as_katago_setting(), "BLACK");
        assert_eq!(Perspective::White.as_katago_setting(), "WHITE");
        assert_eq!(Perspective::SideToMove.as_katago_setting(), "SIDETOMOVE");
    }

    /// Ownership only appears when it was asked for, and an engine that adds
    /// fields must not break jobs already in flight.
    #[test]
    fn a_turn_response_parses_with_and_without_ownership() {
        let bare: TurnResponse = serde_json::from_value(serde_json::json!({
            "id": "q0", "turnNumber": 3,
            "rootInfo": {"winrate": 0.5, "scoreLead": 0.0, "currentPlayer": "B"},
            "moveInfos": [], "somethingNew": 42
        }))
        .expect("unknown fields are tolerated");
        assert!(bare.ownership.is_none());
        assert_eq!(
            bare.root_info.and_then(|root| root.current_player),
            Some(Color::Black)
        );

        let owned: TurnResponse = serde_json::from_value(serde_json::json!({
            "id": "q0", "turnNumber": 0,
            "rootInfo": {"winrate": 0.5, "scoreLead": 0.0, "currentPlayer": "W"},
            "moveInfos": [], "ownership": [0.5, -0.5, 1.0, -1.0]
        }))
        .expect("an ownership map parses");
        assert_eq!(
            owned.ownership.as_deref(),
            Some([0.5, -0.5, 1.0, -1.0].as_slice())
        );
    }

    #[test]
    fn is_ready_line_matches_the_expected_marker_case_insensitively() {
        assert!(is_ready_line(
            "Beginning GTP loop; Ready to begin handling requests"
        ));
        assert!(is_ready_line("READY TO BEGIN HANDLING REQUESTS"));
        assert!(!is_ready_line("Loading model..."));
        assert!(!is_ready_line(""));
    }
}
