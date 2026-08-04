# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`katago-ws` is a queued SGF (Go game record) analysis microservice. `POST /analyse`
validates an SGF, persists a job, and enqueues it; a background worker replays the
game through a [KataGo](https://github.com/lightvector/KataGo) analysis subprocess,
annotates every move, and stores the result. Clients read status via `GET
/analyse/{id}` (optionally long-polling with `?wait=`), list jobs, or tail a live
SSE feed.

It is a thin **consumer of the local `muxa` framework** (path dep at
`../muxa/crates/muxa`). Web server, Diesel/Postgres pool, pgmq, OpenTelemetry,
Sentry, OpenAPI, and rate limiting are all muxa plugins composed in
[`src/main.rs`](src/main.rs); this crate supplies the domain logic (engine, queue,
job store, HTTP surface). `pgmq` is also a local path dep (`../pgmq/pgmq-rs`).

## Commands

Run from the `katago-ws/` directory. The build needs the sibling `../muxa` and
`../pgmq` path-deps present.

```bash
# Run (role = first CLI arg; default `standalone`). Needs Postgres — see below.
cargo run                      # standalone: web + in-process workers
cargo run -- orchestrator      # web + pgmq + cluster dispatcher, no engine
cargo run -- worker            # KataGo + cluster client, no Postgres

cargo build                    # CI/Docker use `cargo build --release --locked`
cargo clippy                   # lints are enforced — see Conventions
just test                      # = cargo nextest run  (install: cargo install cargo-nextest --locked)
just test sgf                  # filter; `just test --no-capture` to see output
cargo test                     # works too (single package, no workspace here)
```

Tests are unit tests colocated in `src/**` (`engine/sgf.rs`, `engine/annotate.rs`,
`http/handlers.rs`, `cluster/{registry,server}.rs`). There is no separate
`tests/` dir; none require a live DB or KataGo.

### Database

A Postgres database named `goban` is required for the `standalone`/`orchestrator`
roles (the `worker` role uses none). Default local URL:
`postgres://postgres:postgres@localhost:5432/goban`. **Migrations are embedded and
applied automatically at startup** (one transaction) — do not run them by hand.
The schema is one `jobs` table plus pgmq's own tables in the `pgmq` schema.

### Local stack & images

```bash
docker compose up --build      # Postgres + the app (CPU KataGo + model bundled, no GPU)
just cpu | just cuda | just opencl   # build a per-platform image (run from katago-ws/;
                                     # build context is the PARENT dir for the path-deps)
```

If you change the sibling `muxa` framework, it does **not** build standalone —
validate muxa changes by building/testing through this crate.

## Architecture

### Launch roles (one binary, three modes)

The role is the **first CLI argument** (or the `KATAGO_WS_ROLE` env var), *not*
config — see `Role::resolve` in [`src/main.rs`](src/main.rs):

- **`standalone`** (default) — web API + in-process worker loops sharing one
  Postgres/pgmq. The original single-process deployment.
- **`orchestrator`** — web API + pgmq + the cluster dispatcher; **no engine**.
  Owns the DB and proxies work to remote workers.
- **`worker`** — runs KataGo and dials the orchestrator; **no Postgres**. Serves
  only `/health`.

`orchestrator` + N `worker`s scale the API and the engine independently when
workers can't reach Postgres. The pgmq lease always stays on the orchestrator, so a
remote worker crash is redelivered exactly like a local one.

### Job lifecycle & the storage model

This is the load-bearing design decision; read [`migrations/.../up.sql`](migrations/2026-05-30-000000_create_job_store/up.sql)
and [`src/db/mod.rs`](src/db/mod.rs) together.

- **The pgmq `analysis` queue is the authority for in-flight work** and the message
  **carries the SGF itself** ([`JobMessage`](src/queue.rs)). A crash replays
  unfinished jobs straight from the durable queue — no other store needed, no orphan
  case.
- **One LOGGED `jobs` table is both the system of record and the realtime read
  model.** A row is updated *in place* across its lifecycle (`queued → running →
  done|failed`). An earlier queue/analyses/job_events split was deliberately
  collapsed into this single table.
- Status is a real Postgres enum (`job_status`), mapped 1:1 to Rust `JobStatus`.
  A `CHECK` enforces result⊕error: terminal rows are exactly success (result, no
  error) or failure (error, no result).
- Every write is stamped by a **DB trigger** with `updated_at` and a monotonic
  `change_seq` (from a sequence) — the app can't forget the cursor.
- **`id` is a time-ordered UUIDv7 minted by Postgres** (the `uuidv7()` DEFAULT;
  hand-rolled because PG<18 lacks it). `db::submit` inserts with `RETURNING id` and
  enqueues in the **same transaction**, so a client never gets an id for a job
  that isn't both visible and queued. Do not generate ids client-side.

### Idempotent, redelivery-safe writes

All state transitions (`set_running`/`set_done`/`set_failed`/`record_attempt_failure`)
are guarded with `.filter(status.eq_any(NON_TERMINAL))`. A job redelivered by the
queue *after it already finished* can't be reverted or double-written. Keep this
guard on any new transition. The worker archives the queue message **only on
success**; on failure it records a *non-terminal* attempt and leaves the message for
pgmq to redeliver, until `max_attempts` is exceeded (then terminal `failed`). A
`VtHeartbeat` re-arms the visibility-timeout lock at half-interval so a long
analysis isn't redelivered mid-run. See [`src/worker.rs`](src/worker.rs).

### Shared worker loop, swappable executor

`worker_loop` (pgmq lease + heartbeat + guarded writes) is **identical** for local
and remote execution; only the [`Executor`](src/worker.rs) trait impl differs —
`LocalExecutor` runs KataGo in-process; the orchestrator runs the same loop per
connected worker with a remote executor that ships the SGF over the cluster socket.
The unit of work is one coarse `analyze(sgf) → GameAnalysis JSON` call.

### Realtime reads come from the DB, never in-process state

`GET /analyses/stream` (SSE) and the `GET /` dashboard read **persisted** job state:
the stream emits a snapshot, then tails `jobs` via the `change_seq` cursor
(`changes_since`). This is what keeps them correct under horizontal scaling — any
instance's worker progress is visible, and a reconnecting client resumes from the DB,
not from lost broadcast state. Do not reintroduce an in-process broadcast channel for
cross-job realtime.

### HTTP surface

[`src/http/`](src/http/mod.rs) declares an aide `ApiRouter`; muxa's `ApiPlugin`
finishes it into the axum router + OpenAPI doc and serves `/docs` + `/openapi.json`.
JSON endpoints use `api_route` (documented); `/`, `/analyses/stream`, `/health`, and
`/cluster` are plain routes (HTML/SSE/WS aren't expressible as JSON schema). aide
types come through the `muxa::aide` re-export — never add a direct `aide` dep.
`POST /analyse` is the only rate-limited endpoint (per-IP, muxa-web layer), in its
own sub-router so the layer applies to it alone.

### Engine

[`src/engine/`](src/engine/mod.rs): `KataGoEnginePlugin` launches `katago analysis
-config <cfg> -model <model>` as a long-lived subprocess and shares it as
`Arc<AnalysisEngine>`. `analyze` = parse SGF (`engine/sgf.rs`) → drive KataGo over
JSON (`engine/katago.rs`) → annotate moves (`engine/annotate.rs`).

Before spawning, `AnalysisEngine::spawn` runs `engine/preflight.rs` (validates
binary/config/model, producing one actionable `AppError::EngineStartup` instead
of a cryptic failure) and, if `[engine].auto_tune`, `engine/tune.rs` (runs
`katago benchmark -tune` once and caches the derived thread/batch settings —
applied via `-override-config`, never by rewriting `analysis.cfg`). `KataGo::spawn`
then waits (bounded by `startup_timeout_secs`, not a failure on expiry — a fresh
GPU host's first-run autotune can take minutes) for KataGo's own ready signal on
stderr before claiming "ready", so a bad model/config/GPU-driver crash is caught
at startup rather than on the first job.

### ⚠️ "gRPC" naming is stale — the transport is WebSocket

The orchestrator↔worker control plane was migrated from gRPC/protobuf to a plain
**WebSocket** in commit `3721721`. The code, docs, and deploy configs have been
brought in line, but older history (and any stray comment) may still say "gRPC" /
port 50051 — that wording is stale. The truth:

- The orchestrator serves the socket at **`GET /cluster` on its web port (3000)** —
  no separate listen port. A worker dials `ws(s)://…/cluster` (`tokio-tungstenite`).
- Wire messages are JSON frames (`src/cluster/message.rs`): worker sends `Hello`
  (name + slots) then `Result`s; orchestrator pushes `JobRequest`s.
- Real config keys: `MUXA_WORKER__ORCHESTRATOR_URL` (the `ws(s)://…/cluster` URL),
  `MUXA_WORKER__AUTH_TOKEN`, and `MUXA_ORCHESTRATOR__AUTH_TOKEN` (shared Bearer
  secret). **There is no `MUXA_ORCHESTRATOR__GRPC_LISTEN` and no port 50051** —
  ignore any such reference if you encounter one.

When writing new code or docs, describe this as WebSocket; don't propagate the
gRPC wording.

## CI/CD & releases

Four workflow files, each with a distinct job:

- [`ci.yml`](.github/workflows/ci.yml) — PR/main-push gate: `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo nextest run`.
- [`pr-title-lint.yml`](.github/workflows/pr-title-lint.yml) — advisory Conventional
  Commits check on PR titles (release-plz parses these under squash-merge).
- [`image.yml`](.github/workflows/image.yml) — the **production-load-bearing** path:
  builds + pushes rolling `:cpu`/`:cuda`/`:opencl`/`:latest`/`:<variant>-<sha>` images
  on every push to `main`. Coolify's deploy tracks `:latest`. Don't touch this file
  casually — a break here is a break in production.
- [`release.yml`](.github/workflows/release.yml) — tag-triggered (`v*.*.*`, pushed by
  release-plz): compiles the binary **once** on the runner (not per-variant, unlike
  `image.yml`), builds `:<variant>-vX.Y.Z` images from that prebuilt binary via the
  Dockerfile's `runtime-prebuilt` target, and attaches a binary tarball + checksum to
  the GitHub Release. Kept in its own file so it can break without risking `image.yml`.

Plus [`release-plz.yml`](.github/workflows/release-plz.yml) (version bump / changelog
PR / tag+release, config in [`release-plz.toml`](release-plz.toml)) — runs on
`RELEASE_PLZ_PAT`, not `GITHUB_TOKEN`, because GitHub suppresses downstream workflow
triggers for `GITHUB_TOKEN`-authored pushes and the `v*.*.*` tag it pushes must fire
`release.yml`.

**The `muxa`/`pgmq` sibling pins are one source of truth**:
[`.github/sibling-refs.env`](.github/sibling-refs.env), read by
[`.github/actions/checkout-siblings/`](.github/actions/checkout-siblings/action.yml)
(used by `ci.yml`, `image.yml`, and `release.yml`'s `build-binary` job — `muxa` used to
float on `ref: main`, which is why this exists). Bumping `muxa`/`pgmq` is a one-line
change there, not a per-workflow edit.

**The KataGo version + per-variant base/zip/pkgs trio live only in the
[`justfile`](justfile)** (`katago_version` + `*_base`/`*_zip`/`*_pkgs`/`opencl_rusticl`
vars). `just print-matrix` emits them as JSON; `image.yml` and `release.yml` both
consume it via a small `matrix` job + `fromJson()` rather than hardcoding a second copy
in YAML. Change the KataGo version in exactly one place: the justfile.

## Conventions

- **Config**: muxa loads `muxa.toml` via figment, merged with `MUXA_*` env vars
  (`__` = table nesting, e.g. `MUXA_ENGINE__MAX_VISITS`, `MUXA_DIESEL__URL`).
  muxa's plugins own `[web]/[otel]/[sentry]/[diesel]`; this crate owns
  `[engine]/[worker]/[orchestrator]/[ratelimit]` (see [`src/config.rs`](src/config.rs)).
  Secrets come from the environment in production, not the committed `muxa.toml`.
  The `muxa.toml` file itself is found via [`src/paths.rs`](src/paths.rs):
  `$MUXA_CONFIG` > `./muxa.toml` > `$XDG_CONFIG_HOME/katago-ws/muxa.toml` > the
  bare default — `main.rs`'s `build_app()` resolves this and passes it to
  muxa's `App::with_config_file`, since `App::default()` only knows about the
  first and second of those. `EngineConfig`'s `config`/`model` defaults follow
  the same CWD-then-XDG-data-dir rule via `paths::data_file`. `main.rs`'s
  `extract::<T>()` helper defaults a config section only when it's genuinely
  absent — a present-but-malformed section (e.g. `concurrency = "two"`) is a
  hard startup error, not a silent fallback.
- **Lints are enforced** (`[lints.clippy]` in [`Cargo.toml`](Cargo.toml), a curated
  `restriction` subset). Notably: `unwrap_used`, `print_stdout`/`print_stderr`
  (use `tracing`, not stdout/stderr), `dbg_macro`, `todo`, `unimplemented`,
  `str_to_string`, `clone_on_ref_ptr`, `min_ident_chars`,
  `allow_attributes_without_reason` (every `#[allow]` needs `reason = "…"`). Tests
  may `.unwrap()` (exempted via `clippy.toml`).
- **Shared mutable state uses the channel/actor pattern, not `Mutex`/`Arc<Mutex>`.**
  The KataGo client and the worker `WorkerRegistry` (`src/cluster/registry.rs`) each
  have one owner task that exclusively holds the state; everything else talks to it
  by message-passing. Follow this for new shared state.
- **The queue name is not configurable** — it's fixed to `queue::Queue::Analysis`
  (`"analysis"`), the single source of truth that `PgmqPlugin` creates and
  `db::submit` produces to, so consumer and producer can't drift.
