# Deploying katago-ws on Cloudflare Containers

This deploys the **`standalone`** role (web API + in-process pgmq worker + KataGo
engine) as a single container fronted by a Worker. Read the **Caveats** first —
Cloudflare Containers is an imperfect fit for this workload.

## Prerequisites

- A **reachable external Postgres** (Neon / Supabase / RDS / a box behind a
  Cloudflare Tunnel). Cloudflare does **not** provide one. pgmq lives inside it;
  the app creates the schema on startup.
- A running **Docker** daemon (wrangler builds the image locally for
  `linux/amd64`), Node 20+, and a Cloudflare account with Containers enabled.

## 1. Set secrets

```sh
npm install
npx wrangler secret put MUXA_DATABASE_URL    # postgres://user:pass@host:5432/dbname
npx wrangler secret put MUXA_SENTRY__DSN      # optional
```

`MUXA_DATABASE_URL` is passed into the container as `MUXA_DIESEL__URL` (the var
the app actually reads — the muxa `[diesel]` section) by `src/index.ts`.
Non-secret config (port, max_visits, RUST_LOG, …) also lives there (`this.envVars`).

## 2. Deploy

```sh
# Run from THIS directory — the image/context paths in wrangler.jsonc are
# relative to it. wrangler builds the Dockerfile (parent context, so it picks up
# the muxa/pgmq crates) AND pushes it to the managed registry, then deploys.
npx wrangler deploy
```

That's it — `wrangler deploy` does the build + push for you (the
`image` + `image_build_context` fields point it at the Dockerfile and the repo's
parent dir). The first build is slow: it compiles the Rust release binary and
downloads KataGo + the ~150 MB model. The image must fit the instance disk
(`standard-4` = 20 GB) — fine, but keep an eye on it.

Test: `curl https://<your-worker>.workers.dev/health` → `ok`, then
`POST /analyse` with an SGF body.

> **CI / pre-built image alternative.** If you'd rather build elsewhere, push with
> `npx wrangler containers push katago-ws:latest` (or to Docker Hub / ECR) and set
> `containers[0].image` to that reference instead of the Dockerfile path.

## Caveats (read these)

1. **Background worker vs. sleep.** Containers sleep after idle. The standalone
   role's queue worker only runs while the container is awake. The Cron keep-alive
   (`*/5 * * * *` in `wrangler.jsonc`) keeps one instance up so the queue drains —
   which makes it **effectively always-on** (you pay for runtime). Drop the cron if
   you'd rather scale to zero and only process while serving requests.
2. **KataGo is slow here.** Capped at 4 vCPU (CPU/Eigen build). `MUXA_ENGINE__MAX_VISITS`
   is set to 10 so a full game fits the 600 s timeout; analysis quality is far below
   a multi-core box. This is fine for demos, weak for real analysis.
3. **Only `standalone` fits.** The `orchestrator`/`worker` cluster split (workers
   dialing the orchestrator's `/cluster` WebSocket) doesn't map to Containers
   (instances are Worker-fronted, with no stable socket for workers to dial back into).
   To scale out, run that split on normal VMs, not multiple of these containers.
4. **External Postgres is mandatory** and must be reachable from Cloudflare's network
   (public, or via Tunnel). The container has normal egress (unlike Workers).
5. **Single instance.** `max_instances: 1` + `getByName("main")` keeps one queue
   worker authoritative. Multiple standalone replicas *can* share pgmq safely, but
   each would need to stay awake to drain it — added cost/complexity for little gain.
6. **Cold start.** KataGo init takes a while on first analysis; the HTTP server
   (which gates `defaultPort`) comes up quickly, so health checks pass before the
   engine is fully warm.
