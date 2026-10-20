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
job store, HTTP surface). `pgmq` is a **git** dep on the Sagebati fork at the same
`rev` muxa pins in its root `Cargo.toml`, so cargo unifies them into one pgmq —
bump the two together. Its `Queue` trait lives at `pgmq::queue::Queue` behind the
`queue-experimental` feature.

## Commands

Run from the `katago-ws/` directory. The build needs the sibling `../muxa` path-dep
present (at the `MUXA_REF` in `.github/sibling-refs.env`).

```bash
# Run (role = first CLI arg; default `standalone`). Needs Postgres — see below.
cargo run                      # standalone: web + in-process workers
cargo run -- orchestrator      # web + pgmq + cluster dispatcher, no engine
cargo run -- worker            # KataGo + cluster client, no Postgres
cargo run --release -- bench   # KataGo only: throughput benchmark, then exit

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

### Launch roles (one binary, four modes)

The role is the **first CLI argument** (or the `KATAGO_WS_ROLE` env var), *not*
config — see `Role::resolve` in [`src/main.rs`](src/main.rs):

- **`standalone`** (default) — web API + in-process worker loops sharing one
  Postgres/pgmq. The original single-process deployment.
- **`orchestrator`** — web API + pgmq + the cluster dispatcher; **no engine**.
  Owns the DB and proxies work to remote workers.
- **`worker`** — runs KataGo and dials the orchestrator; **no Postgres**. Serves
  only `/healthz`.
- **`bench`** — KataGo only (no Postgres, no web): analyzes an SGF directory at
  a fixed concurrency and writes a JSON report ([`src/bench.rs`](src/bench.rs)).
- **`autotune [--fast] [--force]`** — runs the startup auto-tune once into its
  cache and exits (see "Startup auto-tune" below).

`katago-ws healthcheck` is a probe, not a role: it requests this instance's own
`GET /healthz` on `[web].port` and exits 0/1. The images' Docker `HEALTHCHECK`
runs it, so they need no curl/wget ([`src/healthcheck.rs`](src/healthcheck.rs)).

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
JSON endpoints use `api_route` (documented); `/`, `/analyses/stream`, `/healthz`, and
`/cluster` are plain routes (HTML/SSE/WS aren't expressible as JSON schema). aide
types come through the `muxa::aide` re-export — never add a direct `aide` dep.
`POST /analyse` is the only rate-limited endpoint (per-IP, muxa-web layer), in its
own sub-router so the layer applies to it alone. `POST /analyse/batch` queues
work too but skips the IP limit: it's gated by `[api].client_token` (Bearer,
checked in the handler via [`src/auth.rs`](src/auth.rs)) when that is set, and
open to anyone when it isn't — the same convention as the cluster token. `GET /analyses/changes` is the pull form of the stream's `change_seq` tail,
holding back changes under 5 s old so a client cursor can't pass a write that
hasn't committed (sequence numbers are taken at write time, not commit time).

### Engine

[`src/engine/`](src/engine/mod.rs): `KataGoEnginePlugin` launches `katago analysis
-config <cfg> -model <model>` as a long-lived subprocess and shares it as
`Arc<AnalysisEngine>`. `analyze` = parse SGF (`engine/sgf.rs`) → drive KataGo over
JSON (`engine/katago.rs`) → annotate moves (`engine/annotate.rs`).

### Benchmarking the worker

`scripts/bench_setup.sh` fills the gitignored `bench/` (KataGo release binary,
models, an OGS game corpus). `just bench LABEL [katagoKey=value …] [env.KATAGO_WS_X=…]`
runs one benchmark against a derived KataGo config (`just bench-quick` = 6 games,
~1 min, for fast A/B; `just bench-prod` = ~300 games, ~45 min, for final numbers;
`just bench-models` = one run per model in `bench/models/`); `scripts/bench_compare.py
BASE.json RUN.json…` tabulates throughput and the per-move evaluation drift vs a
base run (benchmark the base twice to see the MCTS noise floor). The worker is
GPU-bound: the Rust side is ~0% CPU, so throughput comes from KataGo's settings —
positions in parallel (`[engine] analysis_threads`, 1 search thread each),
`nn_server_threads` × `nn_max_batch_size`, and the OpenCL device/driver. Stock
KataGo's OpenCL backend shares one in-order command queue across NN server
threads, which caps one process well below the GPU's capacity;
[`patches/`](patches/) has a per-thread-queue fix (upstream: lightvector/KataGo#1267).

### Startup auto-tune (`[engine] autotune`, on by default)

Every role that launches KataGo (`standalone`, `worker`, `bench`) first runs
[`engine::autotune`](src/engine/autotune/mod.rs) from `KataGoEnginePlugin`
(whose `read_config` also reads `[worker].concurrency` and which tuning fields
were set explicitly). Each candidate is a fresh KataGo fed an embedded 17-game
corpus in 16-turn chunks at the queue depth of `[worker].concurrency` games;
positions/s is read off a turn counter after a warm-up wave. It picks
`nn_server_threads` × `nn_max_batch_size` (the build-dependent lever: stock
OpenCL KataGo wants 4×64, the `patches/` build 8×32, KataGo's native ROCm/HIP
backend 2×128; candidate order follows the backend `katago version` reports) and
`analysis_threads`; `search_threads_per_analysis` is 1. On OpenCL it reads
KataGo's device list from its startup log and pins a vendor runtime's device
(ROCm "AMD Accelerated Parallel Processing") over Mesa rusticl on every server
thread. Eigen (CPU) builds skip it.
- **Modes** (`[engine] autotune_mode`): `thorough` (default,
  [`thorough.rs`](src/engine/autotune/thorough.rs): every server×batch
  candidate, then 256/128/512 analysis threads, near-ties re-measured; ~1–1.5 min)
  or `fast` ([`fast.rs`](src/engine/autotune/fast.rs), under ~30 s: positions in
  flight from the GPU's SIMD lanes — AMD KFD topology, else `clinfo` — capped by
  the queue depth; only the server-thread split is searched, on a 1/2/4/8/16
  ladder capped at the CPU count, from the backend's usual winner; two trials,
  more only within a 25 s budget). A cached `thorough` result satisfies `fast`.
- **One-shot**: `katago-ws autotune [--fast] [--force]` (`just autotune --fast`)
  tunes into the cache and exits (`AutotunePlugin`).
- **Precedence**: tuning fields given explicitly in the config file or env, and
  any key in `override_config`, are pinned — never changed; only the rest are
  tuned, and the results are written back into the `[engine]` fields (the device
  keys into `override_config`), so `EngineConfig::katago_overrides()` still
  builds the argument. Built-in defaults (`analysis_threads` = CPU count) and an
  explicit `0` don't pin.
- **Cache**: `[engine] autotune_cache` (default `~/.katago/katago-ws-autotune.json`,
  beside KataGo's `opencltuning/`), keyed by GPU PCI ids + OpenCL ICDs, KataGo
  binary/model/config (path, size, mtime), `max_visits`, board size, concurrency,
  mode and the pinned keys. A hit skips tuning; delete the file to retune.
- **Never blocks startup**: no GPU, KataGo dying, or exceeding
  `autotune_timeout_secs` (300; a first run on a new GPU/model includes KataGo's
  ~30–90 s OpenCL kernel tuning) logs a WARN and launches with the config as
  given. `KATAGO_WS_ENGINE__AUTOTUNE=false` disables it.
- **Docker**: persist `/home/app/.katago` (kernel tuning + the cache) in a volume.
- `scripts/bench.sh` runs with auto-tune off (`BENCH_AUTOTUNE=true` to bench the
  tuned settings), so derived-config A/B runs measure exactly what they say.

### ⚠️ "gRPC" naming is stale — the transport is WebSocket

The orchestrator↔worker control plane was migrated from gRPC/protobuf to a plain
**WebSocket** in commit `3721721`. The code, docs, and deploy configs have been
brought in line, but older history (and any stray comment) may still say "gRPC" /
port 50051 — that wording is stale. The truth:

- The orchestrator serves the socket at **`GET /cluster` on its web port (3000)** —
  no separate listen port. A worker dials `ws(s)://…/cluster` (`tokio-tungstenite`).
- Wire messages are JSON frames (`src/cluster/message.rs`): worker sends `Hello`
  (name + slots) then `Result`s; orchestrator pushes `JobRequest`s.
- Real config keys: `KATAGO_WS_WORKER__ORCHESTRATOR_URL` (the `ws(s)://…/cluster` URL),
  `KATAGO_WS_WORKER__AUTH_TOKEN`, and `KATAGO_WS_ORCHESTRATOR__AUTH_TOKEN` (shared Bearer
  secret). **There is no `KATAGO_WS_ORCHESTRATOR__GRPC_LISTEN` and no port 50051** —
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
float on `ref: main`, which is why this exists). Bumping `muxa` is a one-line
change there, not a per-workflow edit. The `pgmq` sibling is still checked out and
copied into the image build context, but nothing path-depends on it any more (see
the git dep above); keep `PGMQ_REF` equal to that rev until the checkout is removed.

**The KataGo version + per-variant base/zip/pkgs trio live only in the
[`justfile`](justfile)** (`katago_version` + `*_base`/`*_zip`/`*_pkgs`/`opencl_rusticl`
vars). `just print-matrix` emits them as JSON; `image.yml` and `release.yml` both
consume it via a small `matrix` job + `fromJson()` rather than hardcoding a second copy
in YAML. Change the KataGo version in exactly one place: the justfile.

## Conventions

- **Config**: `muxa::app!()` names the config after this crate: `config.toml` (or
  `$KATAGO_WS_CONFIG`) merged with `KATAGO_WS_*` env vars (`__` = table nesting, e.g.
  `KATAGO_WS_ENGINE__MAX_VISITS`, `KATAGO_WS_DIESEL__URL`, `KATAGO_WS_ENV`). One prefix
  covers every section; there is no `KATAGO_WS_*` and no `muxa.toml` any more.
  muxa's plugins own `[web]/[otel]/[sentry]/[diesel]`; this crate owns
  `[engine]/[worker]/[orchestrator]/[api]/[ratelimit]` (see [`src/config.rs`](src/config.rs)).
  Secrets come from the environment in production, not the committed `config.toml`.
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
