# katago-ws

A queued **SGF (Go game record) analysis microservice**. You `POST` a game,
[katago-rs](https://github.com/Sagebati/katago-rs) (a Rust port of
[KataGo](https://github.com/lightvector/KataGo)'s analysis engine, on CPU, Vulkan
or ROCm) replays it move-by-move,
and you read back an annotated analysis — every move scored, classified, and
summarised.

It's built on the [`muxa`](https://github.com/Sagebati/muxa) framework: the web
server, Postgres pool, message queue, OpenAPI, rate limiting, and telemetry are
all muxa plugins composed in [`src/main.rs`](src/main.rs); this crate supplies the
domain logic (engine, queue, job store, HTTP surface).

```
   client ──POST /analyse (SGF)──►  ┌─────────────┐  enqueue   ┌──────────┐
                                    │  web tier   │ ─────────► │   pgmq   │
   client ──GET /analyse/{id}───►   │  (axum)     │            │ (queue)  │
                                    └─────────────┘            └────┬─────┘
                                          ▲   reads/writes           │ lease
                                          │                          ▼
                                    ┌─────┴───────┐  analyze   ┌───────────┐
                                    │  Postgres   │ ◄───────── │  worker   │ ──► katago-rs
                                    │ jobs table  │  result    │ loop      │
                                    └─────────────┘            └───────────┘
```

## How it works

- `POST /analyse` validates the SGF, mints a time-ordered UUIDv7 id, and **inserts
  the job + enqueues it on pgmq in one transaction** — a client never gets an id for
  a job that isn't also queued.
- The pgmq `analysis` queue is the **authority for in-flight work**, and the message
  **carries the SGF itself** — a crash replays unfinished jobs from the durable queue,
  no orphan case.
- One LOGGED `jobs` table is **both the system of record and the realtime read
  model**; a row is updated in place across `queued → running → done|failed`. The SSE
  feed and dashboard read this persisted state (via a monotonic `change_seq` cursor),
  so they stay correct under horizontal scaling.
- State transitions are guarded so a job redelivered *after* it finished can't be
  reverted or double-written; a `VtHeartbeat` keeps a long analysis from being
  redelivered mid-run; a poison job is failed after `max_attempts`.

## Launch roles (one binary, three modes)

The role is the **first CLI argument** (or the `KATAGO_WS_ROLE` env var):

| Role | What it runs | Postgres | Engine |
|------|--------------|:--------:|:------:|
| **`standalone`** (default) | web API + in-process worker loops | ✓ | ✓ |
| **`orchestrator`** | web API + pgmq + the cluster dispatcher | ✓ | ✗ |
| **`worker`** | engine + cluster client (dials the orchestrator) | ✗ | ✓ |

`orchestrator` + N `worker`s let the API and the engine scale independently when
workers can't reach Postgres directly. Workers connect over a **WebSocket** at
`GET /cluster` on the orchestrator's web port (a shared Bearer token authenticates
them); the pgmq lease stays on the orchestrator, so a remote worker crash is
redelivered exactly like a local one.

### Flags

The engine roles take flags for the keys most often changed per host, so a worker
can be sized on its command line rather than with env vars:

```bash
katago-ws worker --orchestrator wss://engine.example/cluster --threads 8 --slots 4 --visits 500
```

| Flag | Key | Roles |
|---|---|---|
| `--visits N` | `[engine] max_visits` | standalone, worker, bench, autotune |
| `--threads N` | `[engine] analysis_threads` (`numAnalysisThreads`) | 〃 |
| `--search-threads N` | `[engine] search_threads_per_analysis` | 〃 |
| `--nn-threads N` | `[engine] nn_server_threads` | 〃 |
| `--batch-size N` | `[engine] nn_max_batch_size` | 〃 |
| `--model PATH` / `--human-model PATH` | `[engine] model` / `human_model` | 〃 |
| `--no-autotune` | `[engine] autotune = false` | 〃 |
| `--slots N` | `[worker] concurrency` | standalone, worker, autotune |
| `--orchestrator URL` / `--name NAME` | `[worker] orchestrator_url` / `name` | worker |

A flag wins over the env var and the config file for the same key; one left out
changes nothing. To the auto-tune a flag is as explicit as an env var: `--threads`
pins `numAnalysisThreads` and the rest are still tuned. Secrets (the cluster token,
the database URL) have no flag, since argv is visible to every user on the host,
and neither does the web port, which the image's `HEALTHCHECK` reads from env.
`katago-ws <role> --help` lists them all.

## HTTP API

| Method & path | Description |
|---|---|
| `POST /analyse` | Submit a raw SGF (request body) → `{ job_id, status }` (202). Optional `?human_profile=rank_5k` (see [Human policy](#human-policy)). Rate-limited per IP. |
| `POST /analyse/batch` | `{ "sgfs": [...] }`, up to 100 → one `{ job_id }` or `{ error }` per SGF, in order (202). Each entry is an SGF string or `{ "sgf": "…", "human_profile": "rank_5k" }`. Not IP rate-limited. Needs `Authorization: Bearer <[api].client_token>` when that is set; open to anyone when it isn't. |
| `GET /analyse/{id}` | Job status, and the analysis once `done`. Long-poll with `?wait=<secs>`. |
| `GET /analyses` | List recent jobs. |
| `GET /analyses/stream` | Server-Sent Events: a snapshot then a live tail of job changes. |
| `GET /analyses/changes?since=<cursor>` | Job changes after a cursor, oldest first → `{ changes, cursor, latest, more }`. The pull feed for clients tracking many jobs: one request per poll instead of one per job. Changes are released once 5 s old, so the cursor never skips a write still committing. |
| `GET /` | Plain-HTML status dashboard (workers, running, queue). |
| `GET /workers` | Connected cluster workers (orchestrator). |
| `GET /healthz` | Liveness/readiness probe. |
| `GET /docs`, `GET /openapi.json` | Swagger UI + OpenAPI document (aide). |
| `GET /cluster` | Worker↔orchestrator WebSocket (not for clients). |

```bash
# Submit a game and poll for the result
curl -X POST --data-binary @ear_reddening.sgf http://localhost:3000/analyse
curl 'http://localhost:3000/analyse/<id>?wait=30'
```

## The analysis payload

`GET /analyse/{id}` returns the analysis in `result` once the job is `done`.
`payload_version` says which contract built it (see `PAYLOAD_VERSION` in
[`src/engine/annotate.rs`](src/engine/annotate.rs)); every field added since v2
is optional, so a v2 reader reads a v3 payload unchanged.

Per-turn grids are indexed by **turn**, `0..=moves.len()`: turn `i` is the
position before move `i + 1`. Each is `board_size²` points, row-major from the
top-left (A19 → T1), and all-or-nothing: present for every turn or absent.

| Field | Since | Shape / encoding |
|---|---|---|
| `ownership` | v2 | `i8[turns][size²]`, hundredths, `-100..=100`, always Black-positive |
| `ownership_stdev` | v3 | `u8[turns][size²]`, hundredths, `0..=100`: spread of ownership across the search tree, high = unsettled |
| `policy` | v3 | `u8[turns][size²]`, `round(255·√p)` of the raw network policy for the side to move; decode `(v/255)²`. `0` = illegal (or `p < ~4e-6`). KataGo's pass entry is dropped |
| `human_policy` | v3 | as `policy`, from the human SL network under `human_profile`; only with a profile and a human model |
| `human_profile` | v3 | the profile `human_policy` was computed for; present exactly when `human_policy` is |
| `moves[].prior` | v3 | raw policy of the move played, `[0, 1]`, exact (not the u8 grid) |
| `moves[].human_prior` | v3 | human policy of the move played; only with `human_policy` |
| `moves[].top_moves[].prior` | v1 | raw policy of the candidate |
| `moves[].top_moves[].human_prior` | v3 | human policy of the candidate; only with `human_policy` |
| `engine.ownership_stdev` / `engine.policy` | v3 | whether those grids are present |
| `engine.human_model` | v3 | the human SL network's name (`b18c384nbt-humanv0`), when `human_policy` is present |
| `engine.name` / `engine.version` / `engine.model` | v3 | `katago-rs`, the KataGo version its analysis engine matches (e.g. `1.18.2`), and the net (`kata1-b18c384nbt`) |

Principal variations (`top_moves[].pv`) run to 15 moves by default
(`[engine] analysis_pv_len`), bounded by how deep the search went. A 325-move
19×19 game with every grid is ~1.5 MB of JSON (~280 KB gzipped).
`KATAGO_WS_ENGINE__INCLUDE_OWNERSHIP_STDEV=false` and `__INCLUDE_POLICY=false`
drop those grids; neither costs search time, only payload.

## Human policy

KataGo's human SL network predicts how a player of a given rank would play.
With it loaded, a job that names a `human_profile` gets that player's policy
for every position (`human_policy`) and for each move (`human_prior`).

1. The `vulkan` and `rocm` images bundle the network (`b18c384nbt-humanv0`,
   compiled into katago-rs like the main nets); the `cpu` image doesn't.
2. Point the worker at it: `KATAGO_WS_ENGINE__HUMAN_MODEL=/opt/katago-rs/nets/b18c384nbt-humanv0.bpk`
   (or `--human-model`). katago-rs then runs with `-human-model`, which takes
   roughly another 100 MB of VRAM; the network is evaluated once per position,
   not per visit, so search speed is unchanged. Outside the images, it is the
   `.bpk` from a katago-rs build with the `net-b18c384nbt-humanv0` feature
   (`scripts/bench_setup.sh` puts one in `bench/models/`).
3. Submit with a profile: `POST /analyse?human_profile=rank_5k`, or
   `{"sgf": "…", "human_profile": "rank_5k"}` in a batch.

Profiles (validated at submit; an unknown one is a 400, or that game's `error`
in a batch): `rank_20k` … `rank_1k`, `rank_1d` … `rank_9d`; `rank_<B>_<W>` for
Black's and White's ranks, e.g. `rank_1d_5k`; `preaz_…`, the same with
pre-AlphaZero (2016) openings; `proyear_1800` … `proyear_2023`. One profile per
job. The profile only shapes the reported human policy, not the search.

A worker without a human model analyses such a job without the human fields
and logs a warning, rather than failing it. With several workers, configure the
model on all of them or a job's human policy depends on which one took it.

## Quick start

The build needs the sibling `../muxa` and `../pgmq` path-deps present; running
an engine role needs a katago-rs binary and its nets (`scripts/bench_setup.sh`
builds them from `../katago-rs` into `bench/`).

```bash
# Everything in Docker (Postgres + the app, katago-rs on the CPU + b10 net bundled, no GPU):
docker compose up --build

# Or run locally against your own Postgres (DB named `goban`):
cargo run                   # standalone — migrations apply automatically at startup
cargo run -- orchestrator   # web + pgmq + cluster dispatcher (no engine)
cargo run -- worker         # engine + cluster client (no Postgres)
```

A Postgres database named `goban` is required for `standalone`/`orchestrator`
(default URL `postgres://postgres:postgres@localhost:5432/goban`). Migrations are
embedded and applied at startup — don't run them by hand. The `worker` role needs
no database.

To run a **worker** from the published image — no build, no Postgres, just the engine
dialing the deployed orchestrator over `wss://` (the `worker` is the final image
argument; the engine binary + model are baked in):

```bash
# CPU build — runs anywhere, no GPU:
docker run --rm \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# AMD GPU that ROCm supports (RDNA2+/CDNA) — the KFD + DRM devices and the
# host's video/render groups, :rocm tag:
docker run --rm --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group video | cut -d: -f3)" \
  --group-add "$(getent group render | cut -d: -f3)" \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:rocm worker

# Any other GPU through Vulkan, :vulkan tag — AMD/Intel: the render device;
# NVIDIA: `--gpus all` (NVIDIA Container Toolkit) instead:
docker run --rm --device /dev/dri \
  --group-add "$(getent group render | cut -d: -f3)" \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:vulkan worker
```

The GPU images run b18c384nbt in f16; the CPU image runs b10c128.
[deploy/split/README.md](deploy/split/README.md#picking-a-worker-image) compares them.

`ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster` is the deployed orchestrator (it's
the `[worker].orchestrator_url` default, so a worker dials it even without the env
var); point `KATAGO_WS_WORKER__ORCHESTRATOR_URL` elsewhere to use your own. The
orchestrator runs without auth, so no `KATAGO_WS_WORKER__AUTH_TOKEN` is needed — set one
(matching `[orchestrator].auth_token`) only if you enable it, and front it with TLS
first, since plain `ws://` would send the token in cleartext.

```bash
just check           # what CI runs: cargo fmt --check, clippy -D warnings, nextest
just test            # cargo nextest run  (cargo test works too)
just lint            # cargo clippy --all-targets -D warnings; lints are enforced
```

## Configuration

[`config.toml`](config.toml) (or the file `$KATAGO_WS_CONFIG` points at) is merged
with `KATAGO_WS_*` environment variables (`__` = table nesting), the environment
winning, and [flags](#flags) win over both. Every section uses that one prefix, framework ones (`[web]`, `[diesel]`,
`[sentry]`, `[otel]`) included. Secrets should come from the environment in
production, not the committed file.

| Key env var | Meaning |
|---|---|
| `KATAGO_WS_ENV` | Run mode: `development` / `production` (default: from build profile). Drives Sentry env + sample rates. |
| `KATAGO_WS_DIESEL__URL` | Postgres URL (use Supabase's **session** pooler, port 5432, in prod). |
| `KATAGO_WS_WEB__HOST` / `KATAGO_WS_WEB__PORT` | Bind address (default `0.0.0.0:3000`). |
| `KATAGO_WS_ENGINE__MAX_VISITS` | Visits per move. **Keep modest (8–50)** so a game finishes inside the lease. |
| `KATAGO_WS_ENGINE__BINARY` / `__CONFIG` / `__MODEL` | katago-rs paths (the image sets these). The model is a `.bpk` named after a net compiled into the binary. |
| `KATAGO_WS_ENGINE__BACKEND` / `__FP16` | katago-rs's `-backend` (`flex` = CPU, `wgpu` = Vulkan, `rocm` = HIP; the binary must have it compiled in; `upstream` = a stock KataGo binary, for benchmark comparisons) and f16 convolutions on a GPU backend (`burnUseFP16`). The image sets these. |
| `KATAGO_WS_ENGINE__ANALYSIS_THREADS` / `__SEARCH_THREADS_PER_ANALYSIS` | `numAnalysisThreads` × `numSearchThreadsPerAnalysisThread` (default: system threads × `1`, the throughput preset). See [Throughput tuning](#throughput-tuning). |
| `KATAGO_WS_ENGINE__NN_MAX_BATCH_SIZE` / `__NN_SERVER_THREADS` / `__OVERRIDE_CONFIG` | More engine overrides (`0` / empty = keep `analysis.cfg`). |
| `KATAGO_WS_ENGINE__AUTOTUNE` / `__AUTOTUNE_MODE` | Startup throughput auto-tune: on by default (`false` = off); `thorough` (default) or `fast` (two or three trials, ~35–45 s on b18). See [Throughput tuning](#throughput-tuning). |
| `KATAGO_WS_ENGINE__HUMAN_MODEL` | Human SL network (`-human-model`, a `.bpk`); empty (default) = none. See [Human policy](#human-policy). |
| `KATAGO_WS_ENGINE__INCLUDE_OWNERSHIP_STDEV` / `__INCLUDE_POLICY` / `__ANALYSIS_PV_LEN` | Payload contents: the `ownership_stdev` and `policy` grids (default `true`) and PV length (default `15`). |
| `KATAGO_WS_ENGINE__SCREEN_VISITS` | Two-pass analysis: screen every move at this many visits, re-search only the swings at `MAX_VISITS` (default `0` = off). |
| `KATAGO_WS_WORKER__CONCURRENCY` | Worker lease loops / advertised slots (games in flight per engine; default `4`). |
| `KATAGO_WS_WORKER__ORCHESTRATOR_URL` | `worker` role: the `ws(s)://…/cluster` URL to dial. |
| `KATAGO_WS_WORKER__AUTH_TOKEN` / `KATAGO_WS_ORCHESTRATOR__AUTH_TOKEN` | Shared Bearer secret for `/cluster`. |
| `KATAGO_WS_API__CLIENT_TOKEN` | Bearer secret for `POST /analyse/batch`, which skips the per-IP rate limit. Unset ⇒ open to anyone (so anyone can skip the limit): set it on a public deployment once clients send it. |
| `KATAGO_WS_SENTRY__DSN` | Enable Sentry (see below). |
| `KATAGO_WS_OTEL__ENDPOINT` | OTLP/gRPC collector endpoint (unset ⇒ OTEL export off). |

## Observability

Set **`KATAGO_WS_SENTRY__DSN`** (or the SDK's own `SENTRY_DSN`) and the service
reports, in Sentry, all of:

- **Issues** — panics and `error!` events.
- **Logs** — INFO/WARN/ERROR lines stream to the Logs explorer.
- **Traces + DB spans** — one transaction per HTTP request, named by route
  (`GET /analyse/{id}`), diesel queries as child spans. `config.toml` traces every
  transaction (`KATAGO_WS_SENTRY__TRACES_SAMPLE_RATE` to lower it) except the
  Docker health probe (never) and the polled `GET /analyses/changes` (2%) — see
  `[sentry.transaction_sample_rates]`. Idle queue polls and lock heartbeats aren't
  traced at all.
- **Custom metrics** — dual-emitted (OTEL + Sentry SDK, since Sentry doesn't ingest
  OTLP metrics):
  - `katago_ws.workers.connected` — gauge: connected workers.
  - `katago_ws.jobs.waiting` — gauge: queue depth.
  - `katago_ws.jobs.processed{outcome}` — counter: throughput.
  - `katago_ws.job.duration{outcome}` — histogram: time per job.
  - `katago_ws.job.queue_wait` — histogram: time a job sat queued before a worker
    picked it up. Rising means the workers can't keep up.
  - `katago_ws.moves.analyzed` — counter: moves in completed games. **Its rate is
    the throughput number to watch** (jobs/s isn't: game lengths vary too much).
  - `katago_ws.positions.analyzed{pass}` — counter: positions the engine searched, by
    `pass` (`full`, or `screen`/`refine` with two-pass analysis).
- **Job traces** — each queue delivery is an `analysis_job` transaction (job id,
  attempt), with a `queue.process` span, the DB writes, `katago.analyze` (moves,
  refined turns) and one `katago.pass` span per engine query (pass, positions,
  visits) beneath it. In the `orchestrator` + `worker` split the job request carries
  the trace, so the worker's `cluster.job` (with the engine spans) joins the same
  trace.
- **Queues** — `POST /analyse` enqueues under a `queue.publish` span and the worker
  consumes under `queue.process` (`messaging.*` data: message id, receive latency,
  retry count), which fills Sentry's *Insights → Queues* view: time in queue,
  processing time and retries for the `analysis` queue.

The same metrics/traces/logs also export over **OTLP** if you set
`KATAGO_WS_OTEL__ENDPOINT` to a collector (e.g. `grafana/otel-lgtm`) — useful for
Grafana/Prometheus dashboards.

## Throughput tuning

**Auto-tune.** Before launching the engine on a GPU backend, every engine role
measures a few `numNNServerThreadsPerModel` × `nnMaxBatchSize` /
`numAnalysisThreads` settings on this GPU with short trials and caches the winner
in `~/.katago/katago-ws-autotune.json` (keyed by GPU, binary, model, backend and
precision), so only the first start pays for it (~1.5–2 min, or ~35–45 s
with `KATAGO_WS_ENGINE__AUTOTUNE_MODE=fast`; trials search each position to
`KATAGO_WS_ENGINE__AUTOTUNE_VISITS`, 16, so a wave of positions finishes in
seconds). On an RX 6800 XT with ROCm and
b18c384nbt it picks 8 × 64 with 256 analysis threads. Settings you give
explicitly are kept and only the rest are tuned; `KATAGO_WS_ENGINE__AUTOTUNE=false`
turns it off, and the CPU backend skips it. `katago-ws autotune [--fast] [--force]`
runs it once and exits. In Docker, mount a volume on `/home/app/.katago` so the
result survives restarts.

The analysis engine batches neural-net evaluations across every
position it is searching, so throughput comes from keeping that batch full and not
wasting visits:

- **Many positions, few threads each.** A game goes to the engine as one query covering
  every turn, so hundreds of positions are always queued. The default overrides
  (passed as `-override-config`) search one position per hardware thread the
  process may use (`numAnalysisThreads` = the system's thread count, cgroup limits
  included) with one thread each (`numSearchThreadsPerAnalysisThread=1`), instead of
  the stock config's 2 × 16, whose threads contend over the same tree. On a GPU the
  batch wants more threads than the host has cores: raise `ANALYSIS_THREADS`
  (mid-tier 32–64, strong 64–256) and keep `NN_MAX_BATCH_SIZE` ≥ analysis × search
  threads ÷ GPUs.
- **Keep the queue fed.** `WORKER__CONCURRENCY` (default 4) games in flight per
  engine stops the engine's queue draining at the tail of a game.
- **Two-pass analysis** (`ENGINE__SCREEN_VISITS`, off by default). Every turn is
  screened at `SCREEN_VISITS` (e.g. 16–32); only the turns either side of a move
  whose screened win rate or score swings by `REFINE_WINRATE_DELTA` (0.03) or
  `REFINE_SCORE_DELTA` (1.0 point), in either direction, are re-searched at
  `MAX_VISITS`, ahead of other games in the engine's queue. Quiet moves keep their
  shallow result, so their candidates carry fewer visits; the payload records
  `engine.screen_visits` when this ran. `positions.analyzed{pass}` shows how much
  is refined.

Compare settings by the rate of `katago_ws.moves.analyzed` on the same set of games.

## Building images

One Dockerfile, three katago-rs backends (build context is the **parent** dir so
the sibling `muxa`/`pgmq`/`katago-rs` checkouts are included; the Dockerfile builds
katago-rs and converts its nets from KataGo's `.bin.gz` releases):

```bash
just cpu        # flex — pure CPU, b10c128, runs anywhere (this is :latest on ghcr)
just vulkan     # wgpu — Vulkan (Mesa RADV/ANV, NVIDIA's driver), b18c384nbt f16
just rocm       # HIP — AMD GPUs that ROCm supports, b18c384nbt f16
just all
```

CI ([`.github/workflows/image.yml`](.github/workflows/image.yml)) builds + pushes
all three to `ghcr.io/sagebati/katago-ws` on every push to `main` (`:latest` = CPU),
with katago-rs checked out at the ref pinned in `.github/sibling-refs.env`. A host that can't build the Dockerfile (it needs the
sibling checkouts) just pulls the image.

## Releases

[release-plz](https://release-plz.dev) drives versioning from Conventional Commit PR
titles: merging PRs to `main` keeps a "chore: release vX.Y.Z" PR up to date
(`Cargo.toml` + [`CHANGELOG.md`](CHANGELOG.md)); merging *that* PR cuts a `vX.Y.Z` git
tag and a GitHub Release.

A `vX.Y.Z` tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml),
which compiles the `x86_64-unknown-linux-gnu` binary once and reuses it for:

- Versioned images: `ghcr.io/sagebati/katago-ws:{cpu,vulkan,rocm}-vX.Y.Z`
- A binary asset on the GitHub Release: `katago-ws-x86_64-unknown-linux-gnu.tar.gz`
  (+ a `.sha256` checksum)

This is purely additive — the rolling `:cpu`/`:vulkan`/`:rocm`/`:latest`/`:<variant>-<sha>`
tags that `image.yml` produces on every `main` push, and the Coolify deploy that
tracks `:latest`, are unchanged. Versioned tags exist for traceability and manual
rollback (`docker pull ghcr.io/sagebati/katago-ws:cpu-v1.2.0`), not (yet) for any
automated deploy.

```bash
# Run a worker from any of the three images, pointed at a deployed orchestrator —
# pick the tag for the host's accelerator; the run flags differ per backend.

# CPU — any host, no GPU:
docker run --rm \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# The same, with flags instead of env vars, on 8 CPU threads:
docker run --rm ghcr.io/sagebati/katago-ws:latest worker \
  --orchestrator ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  --visits 20 --threads 8

# AMD GPU (ROCm) — the KFD + DRM devices and the host's video/render groups:
docker run --rm --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group video | cut -d: -f3)" \
  --group-add "$(getent group render | cut -d: -f3)" \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:rocm worker

# NVIDIA GPU (Vulkan) — add `--gpus all` (needs the NVIDIA Container Toolkit):
docker run --rm --gpus all \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:vulkan worker
```

## Deployment

The light **orchestrator** runs on a small always-on host (Postgres can live
elsewhere, e.g. Supabase); **workers dial in** over `wss://` and do the heavy
engine work. See [`deploy/coolify/`](deploy/coolify/) for a Coolify + Supabase
setup, including the Sentry and TLS notes.

> ⚠️ The `/cluster` socket is public when no `KATAGO_WS_ORCHESTRATOR__AUTH_TOKEN` is set —
> any stranger could register as a worker. Set the token (and the matching
> `KATAGO_WS_WORKER__AUTH_TOKEN`) whenever the orchestrator is internet-reachable.

## Project layout

```
src/
  main.rs        role selection + plugin composition (the three run_* fns)
  config.rs      [engine]/[worker]/[orchestrator]/[ratelimit] config
  http/          aide ApiRouter — handlers, DTOs, SSE, the dashboard
  db/            the jobs store (submit, state transitions, queries) + schema
  queue.rs       the pgmq `analysis` queue + JobMessage (carries the SGF)
  worker.rs      worker_loop + Executor trait (Local vs remote) + VtHeartbeat
  engine/        katago-rs subprocess driver: sgf → analysis JSON → annotate
  cluster/       orchestrator↔worker WebSocket (server, client, registry, messages)
  metrics.rs     custom OTEL + Sentry metrics
migrations/      embedded; the single jobs table + job_status enum
```

## Tech

Rust · [axum](https://github.com/tokio-rs/axum) · [diesel-async](https://github.com/weiznich/diesel_async) + Postgres ·
[pgmq](https://github.com/Sagebati/pgmq) · [katago-rs](https://github.com/Sagebati/katago-rs) · [KataGo](https://github.com/lightvector/KataGo) ·
OpenTelemetry · Sentry · [aide](https://github.com/tamasfe/aide) (OpenAPI) ·
on the [`muxa`](https://github.com/Sagebati/muxa) framework.
