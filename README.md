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

The `worker` also takes a `--diagnose` flag (or `KATAGO_WS_DIAGNOSE` env var): instead
of dialing the orchestrator it runs one built-in analysis and exits — `0` on success,
non-zero on failure — a preflight check that KataGo works on the machine.

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
| `GET /health` | Liveness/readiness probe. |
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
cargo run -- worker --diagnose   # one-shot: run a built-in analysis, print the result, exit
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
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e MUXA_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# NVIDIA GPU — add `--gpus all` and use the :cuda tag:
docker run --rm --gpus all \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:cuda worker

# AMD/Intel/NVIDIA GPU via OpenCL — pass the render device and use the :opencl tag:
docker run --rm --device /dev/dri \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:opencl worker
```

`ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster` is the deployed orchestrator (it's
the `[worker].orchestrator_url` default, so a worker dials it even without the env
var); point `MUXA_WORKER__ORCHESTRATOR_URL` elsewhere to use your own. The
orchestrator runs without auth, so no `MUXA_WORKER__AUTH_TOKEN` is needed — set one
(matching `[orchestrator].auth_token`) only if you enable it, and front it with TLS
first, since plain `ws://` would send the token in cleartext.

Before pointing a new machine at the orchestrator, run the image with
**`worker --diagnose`** to confirm KataGo actually works there (GPU drivers, model,
instruction set) — it runs a single built-in analysis and exits non-zero on failure,
so it's a clean smoke test or CI/deploy gate (no orchestrator or Postgres needed):

```bash
docker run --rm --gpus all ghcr.io/sagebati/katago-ws:cuda worker --diagnose
```

On the GPU images, `--diagnose` also performs KataGo's one-time OpenCL/CUDA
**tuning**, which is slow and cached *inside the container*. Drop `--rm`, then
`docker commit` the tuned container into a new image and push it — every worker
started from that image skips re-tuning and is ready instantly:

```bash
# 1. Run the preflight in a named container (no --rm) so it persists after exit.
docker run --name katago-tune --device /dev/dri \
  ghcr.io/sagebati/katago-ws:opencl worker --diagnose

# 2. Snapshot the tuned container into your own image and push it.
docker commit katago-tune ghcr.io/sagebati/katago-ws:opencl-tuned
docker push ghcr.io/sagebati/katago-ws:opencl-tuned
docker rm katago-tune

# 3. Workers from the tuned image start without re-tuning.
docker run --rm --device /dev/dri \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  ghcr.io/sagebati/katago-ws:opencl-tuned worker
```

```bash
just test            # cargo nextest run  (cargo test works too)
cargo clippy         # lints are enforced
```

## Configuration

muxa loads [`muxa.toml`](muxa.toml) via figment, merged with `MUXA_*` environment
variables (`__` = table nesting). Secrets should come from the environment in
production, not the committed file.

| Key env var | Meaning |
|---|---|
| `MUXA_ENV` | Run mode: `development` / `production` (default: from build profile). Drives Sentry env + sample rates. |
| `MUXA_DIESEL__URL` | Postgres URL (use Supabase's **session** pooler, port 5432, in prod). |
| `MUXA_WEB__HOST` / `MUXA_WEB__PORT` | Bind address (default `0.0.0.0:3000`). |
| `MUXA_ENGINE__MAX_VISITS` | KataGo visits per move. **Keep modest (8–50)** so a game finishes inside the lease. |
| `MUXA_ENGINE__BINARY` / `__CONFIG` / `__MODEL` | KataGo paths (the image sets these). |
| `MUXA_WORKER__CONCURRENCY` | Worker lease loops / advertised slots. |
| `MUXA_WORKER__ORCHESTRATOR_URL` | `worker` role: the `ws(s)://…/cluster` URL to dial. |
| `MUXA_WORKER__AUTH_TOKEN` / `MUXA_ORCHESTRATOR__AUTH_TOKEN` | Shared Bearer secret for `/cluster`. |
| `MUXA_SENTRY__DSN` | Enable Sentry (see below). |
| `MUXA_OTEL__ENDPOINT` | OTLP/gRPC collector endpoint (unset ⇒ OTEL export off). |

## Observability

Set **`MUXA_SENTRY__DSN`** and the orchestrator reports, in Sentry, all of:

- **Issues** — panics and `error!` events.
- **Logs** — INFO/WARN/ERROR lines stream to the Logs explorer.
- **Traces + DB spans** — one transaction per HTTP request, diesel queries as child
  spans (sampled at `MUXA_SENTRY__TRACES_SAMPLE_RATE`; defaults to 0.1 in prod, 1.0
  in dev).
- **Custom metrics** — dual-emitted (OTEL + Sentry SDK, since Sentry doesn't ingest
  OTLP metrics):
  - `katago_ws.workers.connected` — gauge: connected workers.
  - `katago_ws.jobs.waiting` — gauge: queue depth.
  - `katago_ws.jobs.processed{outcome}` — counter: throughput.
  - `katago_ws.job.duration{outcome}` — histogram: time per job.

The same metrics/traces/logs also export over **OTLP** if you set
`MUXA_OTEL__ENDPOINT` to a collector (e.g. `grafana/otel-lgtm`) — useful for
Grafana/Prometheus dashboards.

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

```bash
# Run a worker from any of the three images, pointed at a deployed orchestrator —
# pick the tag for the host's accelerator; the run flags differ per backend.

# CPU (Eigen) — any host, no GPU:
docker run --rm \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e MUXA_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:latest worker

# NVIDIA GPU (CUDA) — add `--gpus all` (needs the NVIDIA Container Toolkit):
docker run --rm --gpus all \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e MUXA_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:cuda worker

# AMD/Intel/NVIDIA GPU (OpenCL) — pass the render node `/dev/dri`:
docker run --rm --device /dev/dri \
  -e MUXA_WORKER__ORCHESTRATOR_URL=ws://tfskksv6ajdzz6nyml575ujx.54.36.100.240.sslip.io/cluster \
  -e MUXA_ENGINE__MAX_VISITS=20 \
  ghcr.io/sagebati/katago-ws:opencl worker
```

## Deployment

The light **orchestrator** runs on a small always-on host (Postgres can live
elsewhere, e.g. Supabase); **workers dial in** over `wss://` and do the heavy
KataGo work. See [`deploy/coolify/`](deploy/coolify/) for a Coolify + Supabase
setup, including the Sentry and TLS notes.

> ⚠️ The `/cluster` socket is public when no `MUXA_ORCHESTRATOR__AUTH_TOKEN` is set —
> any stranger could register as a worker. Set the token (and the matching
> `MUXA_WORKER__AUTH_TOKEN`) whenever the orchestrator is internet-reachable.

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
