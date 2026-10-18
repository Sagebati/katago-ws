# katago-ws

A queued **SGF (Go game record) analysis microservice**. You `POST` a game, a
[KataGo](https://github.com/lightvector/KataGo) engine replays it move-by-move,
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
                                    │  Postgres   │ ◄───────── │  worker   │ ──► KataGo
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
| **`worker`** | KataGo + cluster client (dials the orchestrator) | ✗ | ✓ |

`orchestrator` + N `worker`s let the API and the engine scale independently when
workers can't reach Postgres directly. Workers connect over a **WebSocket** at
`GET /cluster` on the orchestrator's web port (a shared Bearer token authenticates
them); the pgmq lease stays on the orchestrator, so a remote worker crash is
redelivered exactly like a local one.

## HTTP API

| Method & path | Description |
|---|---|
| `POST /analyse` | Submit a raw SGF (request body) → `{ job_id, status }` (202). Rate-limited per IP. |
| `GET /analyse/{id}` | Job status, and the analysis once `done`. Long-poll with `?wait=<secs>`. |
| `GET /analyses` | List recent jobs. |
| `GET /analyses/stream` | Server-Sent Events: a snapshot then a live tail of job changes. |
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

## Quick start

The build needs the sibling `../muxa` and `../pgmq` path-deps present.

```bash
# Everything in Docker (Postgres + the app, CPU KataGo + model bundled, no GPU):
docker compose up --build

# Or run locally against your own Postgres (DB named `goban`):
cargo run                   # standalone — migrations apply automatically at startup
cargo run -- orchestrator   # web + pgmq + cluster dispatcher (no engine)
cargo run -- worker         # KataGo + cluster client (no Postgres)
```

A Postgres database named `goban` is required for `standalone`/`orchestrator`
(default URL `postgres://postgres:postgres@localhost:5432/goban`). Migrations are
embedded and applied at startup — don't run them by hand. The `worker` role needs
no database.

To run a **worker** from the published image — no build, no Postgres, just KataGo
dialing the deployed orchestrator over `wss://` (the `worker` is the final image
argument; the engine binary + model are baked in):

```bash
# CPU (Eigen) build — runs anywhere, no GPU:
docker run --rm \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# NVIDIA GPU — add `--gpus all` and use the :cuda tag:
docker run --rm --gpus all \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:cuda worker

# AMD/Intel GPU via OpenCL — pass the render device and use the :opencl tag:
docker run --rm --device /dev/dri \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:opencl worker
```

The OpenCL image drives the GPU through Mesa's **rusticl** (`RUSTICL_ENABLE=radeonsi,iris`
is baked in; override the build arg for another gallium driver). Mesa's older
Clover is deliberately absent — it can't build KataGo's kernels on RDNA2 and
newer. NVIDIA cards should use the `:cuda` tag rather than OpenCL.

`ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster` is the deployed orchestrator (it's
the `[worker].orchestrator_url` default, so a worker dials it even without the env
var); point `KATAGO_WS_WORKER__ORCHESTRATOR_URL` elsewhere to use your own. The
orchestrator runs without auth, so no `KATAGO_WS_WORKER__AUTH_TOKEN` is needed — set one
(matching `[orchestrator].auth_token`) only if you enable it, and front it with TLS
first, since plain `ws://` would send the token in cleartext.

```bash
just test            # cargo nextest run  (cargo test works too)
cargo clippy         # lints are enforced
```

## Configuration

[`config.toml`](config.toml) (or the file `$KATAGO_WS_CONFIG` points at) is merged
with `KATAGO_WS_*` environment variables (`__` = table nesting), the environment
winning. Every section uses that one prefix, framework ones (`[web]`, `[diesel]`,
`[sentry]`, `[otel]`) included. Secrets should come from the environment in
production, not the committed file.

| Key env var | Meaning |
|---|---|
| `KATAGO_WS_ENV` | Run mode: `development` / `production` (default: from build profile). Drives Sentry env + sample rates. |
| `KATAGO_WS_DIESEL__URL` | Postgres URL (use Supabase's **session** pooler, port 5432, in prod). |
| `KATAGO_WS_WEB__HOST` / `KATAGO_WS_WEB__PORT` | Bind address (default `0.0.0.0:3000`). |
| `KATAGO_WS_ENGINE__MAX_VISITS` | KataGo visits per move. **Keep modest (8–50)** so a game finishes inside the lease. |
| `KATAGO_WS_ENGINE__BINARY` / `__CONFIG` / `__MODEL` | KataGo paths (the image sets these). |
| `KATAGO_WS_ENGINE__ANALYSIS_THREADS` / `__SEARCH_THREADS_PER_ANALYSIS` | KataGo `numAnalysisThreads` × `numSearchThreadsPerAnalysisThread` (default: system threads × `1`, the throughput preset). See [Throughput tuning](#throughput-tuning). |
| `KATAGO_WS_ENGINE__NN_MAX_BATCH_SIZE` / `__NN_SERVER_THREADS` / `__OVERRIDE_CONFIG` | More KataGo overrides (`0` / empty = keep `analysis.cfg`). |
| `KATAGO_WS_ENGINE__AUTOTUNE` / `__AUTOTUNE_MODE` | Startup throughput auto-tune: on by default (`false` = off); `thorough` (default) or `fast` (under ~30 s). See [Throughput tuning](#throughput-tuning). |
| `KATAGO_WS_ENGINE__SCREEN_VISITS` | Two-pass analysis: screen every move at this many visits, re-search only the swings at `MAX_VISITS` (default `0` = off). |
| `KATAGO_WS_WORKER__CONCURRENCY` | Worker lease loops / advertised slots (games in flight per engine; default `4`). |
| `KATAGO_WS_WORKER__ORCHESTRATOR_URL` | `worker` role: the `ws(s)://…/cluster` URL to dial. |
| `KATAGO_WS_WORKER__AUTH_TOKEN` / `KATAGO_WS_ORCHESTRATOR__AUTH_TOKEN` | Shared Bearer secret for `/cluster`. |
| `KATAGO_WS_SENTRY__DSN` | Enable Sentry (see below). |
| `KATAGO_WS_OTEL__ENDPOINT` | OTLP/gRPC collector endpoint (unset ⇒ OTEL export off). |

## Observability

Set **`KATAGO_WS_SENTRY__DSN`** and the orchestrator reports, in Sentry, all of:

- **Issues** — panics and `error!` events.
- **Logs** — INFO/WARN/ERROR lines stream to the Logs explorer.
- **Traces + DB spans** — one transaction per HTTP request, diesel queries as child
  spans (sampled at `KATAGO_WS_SENTRY__TRACES_SAMPLE_RATE`; defaults to 0.1 in prod, 1.0
  in dev).
- **Custom metrics** — dual-emitted (OTEL + Sentry SDK, since Sentry doesn't ingest
  OTLP metrics):
  - `katago_ws.workers.connected` — gauge: connected workers.
  - `katago_ws.jobs.waiting` — gauge: queue depth.
  - `katago_ws.jobs.processed{outcome}` — counter: throughput.
  - `katago_ws.job.duration{outcome}` — histogram: time per job.
  - `katago_ws.moves.analyzed` — counter: moves in completed games. **Its rate is
    the throughput number to watch** (jobs/s isn't: game lengths vary too much).
  - `katago_ws.positions.analyzed{pass}` — counter: positions KataGo searched, by
    `pass` (`full`, or `screen`/`refine` with two-pass analysis).
- **Job traces** — each queue delivery is an `analysis_job` transaction (job id,
  attempt), with the DB writes, `katago.analyze` (moves, refined turns) and one
  `katago.pass` span per KataGo query (pass, positions, visits) beneath it. In the
  `orchestrator` + `worker` split, the engine spans are traced on the worker.

The same metrics/traces/logs also export over **OTLP** if you set
`KATAGO_WS_OTEL__ENDPOINT` to a collector (e.g. `grafana/otel-lgtm`) — useful for
Grafana/Prometheus dashboards.

## Throughput tuning

**Auto-tune.** Before launching KataGo, every engine role measures a few
`numNNServerThreadsPerModel` × `nnMaxBatchSize` / `numAnalysisThreads` settings on
this GPU with short trials, picks the OpenCL device (AMD's ROCm runtime over Mesa
rusticl when both expose the card), and caches the winner in
`~/.katago/katago-ws-autotune.json`, so only the first start pays for it (~1–1.5 min,
or under ~30 s with `KATAGO_WS_ENGINE__AUTOTUNE_MODE=fast`). Settings you give
explicitly are kept and only the rest are tuned; `KATAGO_WS_ENGINE__AUTOTUNE=false`
turns it off. `katago-ws autotune [--fast] [--force]` runs it once and exits. In
Docker, mount a volume on `/home/app/.katago` so the result (and KataGo's own
OpenCL kernel tuning) survives restarts.

KataGo's analysis engine already batches neural-net evaluations across every
position it is searching, so throughput comes from keeping that batch full and not
wasting visits:

- **Many positions, few threads each.** A game goes to KataGo as one query covering
  every turn, so hundreds of positions are always queued. The default overrides
  (passed as `-override-config`) search one position per hardware thread the
  process may use (`numAnalysisThreads` = the system's thread count, cgroup limits
  included) with one thread each (`numSearchThreadsPerAnalysisThread=1`), instead of
  the stock config's 2 × 16, whose threads contend over the same tree. On a GPU the
  batch wants more threads than the host has cores: raise `ANALYSIS_THREADS`
  (mid-tier 32–64, strong 64–256) and keep `NN_MAX_BATCH_SIZE` ≥ analysis × search
  threads ÷ GPUs.
- **Keep the queue fed.** `WORKER__CONCURRENCY` (default 4) games in flight per
  engine stops KataGo's queue draining at the tail of a game.
- **Two-pass analysis** (`ENGINE__SCREEN_VISITS`, off by default). Every turn is
  screened at `SCREEN_VISITS` (e.g. 16–32); only the turns either side of a move
  whose screened win rate or score swings by `REFINE_WINRATE_DELTA` (0.03) or
  `REFINE_SCORE_DELTA` (1.0 point), in either direction, are re-searched at
  `MAX_VISITS`, ahead of other games in KataGo's queue. Quiet moves keep their
  shallow result, so their candidates carry fewer visits; the payload records
  `engine.screen_visits` when this ran. `positions.analyzed{pass}` shows how much
  is refined.

Compare settings by the rate of `katago_ws.moves.analyzed` on the same set of games.

## Building images

One Dockerfile, three KataGo backends (build context is the **parent** dir so the
`muxa`/`pgmq` path-deps are included):

```bash
just cpu        # Eigen — pure CPU, runs anywhere (this is :latest on ghcr)
just cuda       # NVIDIA — run with `--gpus all`
just opencl     # AMD/Intel/NVIDIA via OpenCL — run with `--device /dev/dri`
just all
```

CI ([`.github/workflows/image.yml`](.github/workflows/image.yml)) builds + pushes
all three to `ghcr.io/sagebati/katago-ws` on every push to `main` (`:latest` = CPU).
A host that can't build the Dockerfile (it needs the sibling crates) just pulls the
image.

## Releases

[release-plz](https://release-plz.dev) drives versioning from Conventional Commit PR
titles: merging PRs to `main` keeps a "chore: release vX.Y.Z" PR up to date
(`Cargo.toml` + [`CHANGELOG.md`](CHANGELOG.md)); merging *that* PR cuts a `vX.Y.Z` git
tag and a GitHub Release.

A `vX.Y.Z` tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml),
which compiles the `x86_64-unknown-linux-gnu` binary once and reuses it for:

- Versioned images: `ghcr.io/sagebati/katago-ws:{cpu,cuda,opencl}-vX.Y.Z`
- A binary asset on the GitHub Release: `katago-ws-x86_64-unknown-linux-gnu.tar.gz`
  (+ a `.sha256` checksum)

This is purely additive — the rolling `:cpu`/`:cuda`/`:opencl`/`:latest`/`:<variant>-<sha>`
tags that `image.yml` produces on every `main` push, and the Coolify deploy that
tracks `:latest`, are unchanged. Versioned tags exist for traceability and manual
rollback (`docker pull ghcr.io/sagebati/katago-ws:cpu-v1.2.0`), not (yet) for any
automated deploy.

```bash
# Run a worker from any of the three images, pointed at a deployed orchestrator —
# pick the tag for the host's accelerator; the run flags differ per backend.

# CPU (Eigen) — any host, no GPU:
docker run --rm \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# NVIDIA GPU (CUDA) — add `--gpus all` (needs the NVIDIA Container Toolkit):
docker run --rm --gpus all \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:cuda worker

# AMD/Intel GPU (OpenCL) — pass the render node `/dev/dri`:
docker run --rm --device /dev/dri \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e KATAGO_WS_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:opencl worker
```

## Deployment

The light **orchestrator** runs on a small always-on host (Postgres can live
elsewhere, e.g. Supabase); **workers dial in** over `wss://` and do the heavy
KataGo work. See [`deploy/coolify/`](deploy/coolify/) for a Coolify + Supabase
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
  engine/        KataGo subprocess driver: sgf → katago JSON → annotate
  cluster/       orchestrator↔worker WebSocket (server, client, registry, messages)
  metrics.rs     custom OTEL + Sentry metrics
migrations/      embedded; the single jobs table + job_status enum
```

## Tech

Rust · [axum](https://github.com/tokio-rs/axum) · [diesel-async](https://github.com/weiznich/diesel_async) + Postgres ·
[pgmq](https://github.com/Sagebati/pgmq) · [KataGo](https://github.com/lightvector/KataGo) ·
OpenTelemetry · Sentry · [aide](https://github.com/tamasfe/aide) (OpenAPI) ·
on the [`muxa`](https://github.com/Sagebati/muxa) framework.
