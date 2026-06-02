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
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::EngineConfig;
use crate::engine::sgf::{ColoredMove, ParsedGame, Rules};
use crate::error::{AppError, AppResult};

/// Buffer size of the request channel (callers → owner task). Caps how many
/// submitted-but-unstarted queries queue up before `analyze` callers await.
const CMD_QUEUE_CAPACITY: usize = 64;

/// Buffer size of the line channel (stdout reader → owner task). A whole-game
/// query emits one response line per turn (hundreds), so this absorbs a burst
/// without back-pressuring the reader.
const LINE_QUEUE_CAPACITY: usize = 1024;

/// KataGo `rootInfo` for one analyzed turn (position evaluation).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInfo {
    /// Win probability for the side to move, in `[0, 1]`.
    pub winrate: f32,
    /// Expected score lead (points) for the side to move.
    #[serde(default)]
    pub score_lead: f32,
}

/// KataGo `moveInfos` entry — one candidate move.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveInfo {
    /// Move in GTP notation. (Not camelCase — KataGo's key is literally `move`.)
    #[serde(rename = "move")]
    pub mv: String,
    /// Win probability after this move (side-to-move perspective).
    pub winrate: f32,
    /// Score lead after this move.
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
    /// Spawn the analysis engine. Fails clearly if the binary/model are absent.
    pub async fn spawn(cfg: &EngineConfig) -> AppResult<Arc<Self>> {
        let mut child = Command::new(&cfg.binary)
            .arg("analysis")
            .arg("-config")
            .arg(&cfg.config)
            .arg("-model")
            .arg(&cfg.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                AppError::ModelLoad(format!(
                    "failed to launch '{} analysis': {err} (is KataGo installed?)",
                    cfg.binary
                ))
            })?;

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

        // Surface KataGo's stderr (startup diagnostics, tuning, errors).
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "katago", "{line}");
            }
        });

        // Owner: the sole holder of stdin, the pending table, the id counter,
        // and the child — so the whole protocol runs without locks.
        tokio::spawn(owner_loop(child, stdin, cmd_rx, line_rx));

        Ok(Arc::new(Self {
            cmd_tx,
            timeout: Duration::from_secs(cfg.request_timeout_secs.max(1)),
        }))
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

/// The single owner of stdin and the pending table. Assigns ids, writes
/// queries, and resolves replies as turn responses arrive — all from one task,
/// so the state is plain (no `Mutex`/`Arc`/atomics).
///
/// `child` is held only to keep the subprocess alive: when every [`KataGo`]
/// handle is dropped, `cmd_rx` closes, this loop ends, and dropping `child`
/// triggers `kill_on_drop`.
async fn owner_loop(
    _child: Child,
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
                    // stdout closed → process gone; fail everything outstanding.
                    for (_, req) in pending.drain() {
                        let _ = req.tx.send(Err("katago process exited".into()));
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
