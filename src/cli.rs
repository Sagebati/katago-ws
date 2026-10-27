//! The command line: the launch role, and flags that set config keys.
//!
//! Each flag stands for one `[engine]` or `[worker]` key and reaches the config
//! as the last figment layer, so precedence is flag > `KATAGO_WS_*` env >
//! config file > built-in default. A flag left out leaves its key alone.
//! Because they land in the same figment, flags count as explicit for the
//! auto-tune just as env vars do: `--threads 8` pins `numAnalysisThreads`
//! exactly as `KATAGO_WS_ENGINE__ANALYSIS_THREADS=8` does.
//!
//! Secrets (`[worker] auth_token`, the database URL) get no flag: argv is
//! readable by every user on the host (`ps`), so they stay in env or the file.
//! Neither does `[web] port`: the image's `HEALTHCHECK` runs `katago-ws
//! healthcheck`, which reads the port from env/file and would miss a flag.

use clap::{Args, Parser, Subcommand};
use figment::providers::Serialized;
use serde::{Serialize, Serializer};

/// Env var naming the role when argv has none — for a compose service that
/// keeps the image's `ENTRYPOINT` and sets no `command`.
const ROLE_ENV: &str = "KATAGO_WS_ROLE";

#[derive(Debug, Parser)]
#[command(
    name = "katago-ws",
    version,
    about = "Queued SGF analysis on katago-rs (KataGo's analysis engine in Rust)",
    after_help = "Flags override the config file and KATAGO_WS_* env vars. \
                  The role can also come from KATAGO_WS_ROLE (default: standalone)."
)]
struct Cli {
    #[command(subcommand)]
    role: Option<Role>,
}

/// Which slice of the system this process runs, with its flags.
#[derive(Debug, Subcommand)]
pub enum Role {
    /// Web API + in-process workers sharing one Postgres (the default).
    Standalone {
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        slots: SlotsArgs,
    },
    /// Web API + queue + cluster dispatcher; runs no engine.
    Orchestrator,
    /// The engine + a client dialing the orchestrator; no Postgres.
    Worker {
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        worker: WorkerArgs,
    },
    /// The engine only: a throughput benchmark over an SGF corpus, then exit.
    Bench {
        #[command(flatten)]
        engine: EngineArgs,
    },
    /// Probe this instance's GET /healthz and exit 0 or 1 (Docker HEALTHCHECK).
    Healthcheck,
    /// Auto-tune the engine's throughput settings into the cache, then exit.
    Autotune {
        /// The fast search (two or three trials) instead of `[engine] autotune_mode`.
        #[arg(long)]
        fast: bool,
        /// Re-tune even when a cached result exists.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        slots: SlotsArgs,
    },
}

/// `[engine]` keys. Field names are the config keys; the flags may differ.
#[derive(Debug, Default, Args, Serialize)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Max visits per analysed position ([engine] max_visits).
    #[arg(long = "visits", value_name = "N")]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_visits: Option<u32>,
    /// katago-rs compute backend: flex (CPU), wgpu (Vulkan) or rocm (AMD HIP); upstream for a
    /// stock KataGo binary, for comparisons ([engine] backend).
    #[arg(long, value_name = "BACKEND", value_parser = ["flex", "wgpu", "rocm", "upstream"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    /// Run the GPU network's 3x3 convolutions in f16 ([engine] fp16 = true).
    #[arg(long)]
    #[serde(skip_serializing_if = "is_false")]
    fp16: bool,
    /// Positions searched in parallel, numAnalysisThreads
    /// ([engine] analysis_threads). Default: one per CPU thread, or auto-tuned.
    #[arg(long = "threads", value_name = "N")]
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis_threads: Option<u32>,
    /// Threads per position's search tree, numSearchThreadsPerAnalysisThread
    /// ([engine] search_threads_per_analysis).
    #[arg(long = "search-threads", value_name = "N")]
    #[serde(skip_serializing_if = "Option::is_none")]
    search_threads_per_analysis: Option<u32>,
    /// NN server threads, numNNServerThreadsPerModel: one per GPU, or on the
    /// CPU build the threads running the network ([engine] nn_server_threads).
    #[arg(long = "nn-threads", value_name = "N")]
    #[serde(skip_serializing_if = "Option::is_none")]
    nn_server_threads: Option<u32>,
    /// Max positions per NN batch, per GPU ([engine] nn_max_batch_size).
    #[arg(long = "batch-size", value_name = "N")]
    #[serde(skip_serializing_if = "Option::is_none")]
    nn_max_batch_size: Option<u32>,
    /// Network weights, a katago-rs .bpk ([engine] model).
    #[arg(long, value_name = "PATH")]
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Human SL network weights (.bpk), for jobs with a human_profile
    /// ([engine] human_model).
    #[arg(long, value_name = "PATH")]
    #[serde(skip_serializing_if = "Option::is_none")]
    human_model: Option<String>,
    /// Skip the startup auto-tune ([engine] autotune = false).
    #[arg(long)]
    #[serde(
        rename = "autotune",
        serialize_with = "negated",
        skip_serializing_if = "is_false"
    )]
    no_autotune: bool,
}

/// `[worker] concurrency`, shared by every role that leases games.
#[derive(Debug, Default, Args, Serialize)]
pub struct SlotsArgs {
    /// Games in flight at once, all fed to the one engine process
    /// ([worker] concurrency).
    #[arg(long = "slots", value_name = "N", help_heading = "Worker")]
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<usize>,
}

/// `[worker]` keys for the `worker` role.
#[derive(Debug, Default, Args, Serialize)]
#[command(next_help_heading = "Worker")]
pub struct WorkerArgs {
    #[command(flatten)]
    #[serde(flatten)]
    slots: SlotsArgs,
    /// The orchestrator's cluster socket, e.g. wss://host/cluster
    /// ([worker] orchestrator_url).
    #[arg(long = "orchestrator", value_name = "URL")]
    #[serde(skip_serializing_if = "Option::is_none")]
    orchestrator_url: Option<String>,
    /// Name to register under; empty generates one ([worker] name).
    #[arg(long, value_name = "NAME")]
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// The flags as a config layer: `{ engine: {..}, worker: {..} }`, only the
/// sections and keys that were given.
#[derive(Debug, Serialize)]
struct Layer<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    engine: Option<&'a EngineArgs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker: Option<WorkerLayer<'a>>,
}

/// Whichever flavour of `[worker]` flags the role has.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WorkerLayer<'a> {
    Slots(&'a SlotsArgs),
    Worker(&'a WorkerArgs),
}

impl Role {
    /// The role from argv, else [`ROLE_ENV`], else `standalone`. Exits with
    /// clap's usage message on a bad role or flag, and after `--help`.
    pub fn resolve() -> Self {
        if let Some(role) = Cli::parse().role {
            return role;
        }
        match std::env::var(ROLE_ENV) {
            // Parsed as if typed, so a bad value fails like a bad argument.
            Ok(raw) => Cli::parse_from(["katago-ws", raw.as_str()])
                .role
                .unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Short name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Standalone { .. } => "standalone",
            Self::Orchestrator => "orchestrator",
            Self::Worker { .. } => "worker",
            Self::Bench { .. } => "bench",
            Self::Healthcheck => "healthcheck",
            Self::Autotune { .. } => "autotune",
        }
    }

    /// The figment layer the flags make; merge it over muxa's file + env.
    pub fn overrides(&self) -> Serialized<impl Serialize + '_> {
        let layer = match self {
            Self::Standalone { engine, slots } | Self::Autotune { engine, slots, .. } => Layer {
                engine: Some(engine),
                worker: Some(WorkerLayer::Slots(slots)),
            },
            Self::Worker { engine, worker } => Layer {
                engine: Some(engine),
                worker: Some(WorkerLayer::Worker(worker)),
            },
            Self::Bench { engine } => Layer {
                engine: Some(engine),
                worker: None,
            },
            Self::Orchestrator | Self::Healthcheck => Layer {
                engine: None,
                worker: None,
            },
        };
        Serialized::defaults(layer)
    }
}

impl Default for Role {
    fn default() -> Self {
        Self::Standalone {
            engine: EngineArgs::default(),
            slots: SlotsArgs::default(),
        }
    }
}

/// `--no-autotune` is stored as `autotune = false`.
fn negated<S: Serializer>(value: &bool, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bool(!value)
}

/// Only serialize `--no-autotune` when it was given.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if signature"
)]
fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use figment::Figment;
    use serde_json::json;

    use super::*;
    use crate::config::{EngineConfig, WorkerConfig};

    fn parse(args: &[&str]) -> Role {
        let argv = std::iter::once("katago-ws").chain(args.iter().copied());
        Cli::try_parse_from(argv).unwrap().role.unwrap_or_default()
    }

    /// `below` (standing in for the file + env) with `args`' flags over it.
    fn layered(below: serde_json::Value, args: &[&str]) -> Figment {
        Figment::from(Serialized::defaults(below)).merge(parse(args).overrides())
    }

    #[test]
    fn worker_flags_set_their_config_keys() {
        let fig = layered(
            json!({}),
            &[
                "worker",
                "--threads",
                "8",
                "--visits",
                "500",
                "--slots",
                "4",
                "--orchestrator",
                "wss://engine.example/cluster",
            ],
        );
        let engine: EngineConfig = fig.extract_inner("engine").unwrap();
        let worker: WorkerConfig = fig.extract_inner("worker").unwrap();
        assert_eq!(engine.analysis_threads, 8);
        assert_eq!(engine.max_visits, 500);
        assert_eq!(worker.concurrency, 4);
        assert_eq!(worker.orchestrator_url, "wss://engine.example/cluster");
    }

    #[test]
    fn backend_and_fp16_flags_set_the_engine_keys() {
        let fig = layered(json!({}), &["worker", "--backend", "rocm", "--fp16"]);
        let engine: EngineConfig = fig.extract_inner("engine").unwrap();
        assert_eq!(engine.backend, crate::config::EngineBackend::Rocm);
        assert!(engine.fp16);

        let unset: EngineConfig = layered(json!({}), &["worker"])
            .extract_inner("engine")
            .unwrap();
        assert_eq!(unset.backend, crate::config::EngineBackend::Flex);
        assert!(!unset.fp16);
        assert!(Cli::try_parse_from(["katago-ws", "worker", "--backend", "cuda"]).is_err());
    }

    #[test]
    fn a_flag_beats_env_and_an_absent_one_leaves_it() {
        let below = json!({ "engine": { "analysis_threads": 2, "max_visits": 300 } });
        let fig = layered(below, &["worker", "--threads", "8"]);
        assert_eq!(
            fig.extract_inner::<u32>("engine.analysis_threads").unwrap(),
            8
        );
        assert_eq!(fig.extract_inner::<u32>("engine.max_visits").unwrap(), 300);
    }

    #[test]
    fn absent_flags_stay_absent_so_the_autotune_still_tunes_them() {
        // The auto-tune pins a field when its key is present at all.
        let fig = layered(json!({}), &["worker", "--threads", "8"]);
        assert!(fig.find_value("engine.analysis_threads").is_ok());
        assert!(fig.find_value("engine.nn_server_threads").is_err());
        assert!(fig.find_value("engine.autotune").is_err());
        assert!(fig.find_value("worker.concurrency").is_err());
    }

    #[test]
    fn no_autotune_turns_it_off() {
        let fig = layered(
            json!({ "engine": { "autotune": true } }),
            &["worker", "--no-autotune"],
        );
        assert!(!fig.extract_inner::<bool>("engine.autotune").unwrap());
    }

    #[test]
    fn autotune_keeps_its_own_flags_beside_the_engine_ones() {
        let role = parse(&["autotune", "--fast", "--force", "--slots", "6"]);
        assert!(matches!(
            role,
            Role::Autotune {
                fast: true,
                force: true,
                ..
            }
        ));
        let fig = Figment::from(role.overrides());
        assert_eq!(fig.extract_inner::<usize>("worker.concurrency").unwrap(), 6);
    }

    #[test]
    fn no_role_is_standalone() {
        assert!(matches!(parse(&[]), Role::Standalone { .. }));
    }

    #[test]
    fn a_flag_on_a_role_that_has_no_engine_is_refused() {
        let argv = ["katago-ws", "orchestrator", "--threads", "8"];
        assert!(Cli::try_parse_from(argv).is_err());
        assert!(Cli::try_parse_from(["katago-ws", "worker", "--threads", "many"]).is_err());
    }
}
