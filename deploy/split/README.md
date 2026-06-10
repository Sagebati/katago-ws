# Split deployment: orchestrator + workers on separate machines

The `orchestrator` owns Postgres + the queue and dispatches analysis to remote
`worker` machines over gRPC. Workers run KataGo and **never touch Postgres** —
they only dial the orchestrator. This is the topology the gRPC split was built
for (and why it doesn't fit Cloudflare Containers).

```
            HTTP :3000                    gRPC :50051 (bidi stream)
  clients ───────────►  ORCHESTRATOR  ◄──────────────────────  WORKER host A (xN engines)
  (POST /analyse)       + Postgres     ◄──────────────────────  WORKER host B
                        (lease/redelivery stays here)           WORKER host C …
```

## 1. Build the image once, push to a registry

The Dockerfile needs a **parent** build context (it COPYs the sibling `muxa` and
`pgmq` crates), so build from the directory that contains them:

```sh
cd /path/to/rust            # contains katago-ws/, muxa/, pgmq/
docker build --platform linux/amd64 -f katago-ws/Dockerfile \
  -t registry.example.com/katago-ws:latest .
docker push registry.example.com/katago-ws:latest
```

One image serves every role — the role is just the first CLI arg (the compose
`command:`). Use that `KATAGO_WS_IMAGE` value on all machines.

## 2. Orchestrator machine

```sh
KATAGO_WS_IMAGE=registry.example.com/katago-ws:latest \
POSTGRES_PASSWORD=$(openssl rand -hex 16) \
docker compose -f orchestrator.compose.yml up -d
```

Exposes `:3000` (HTTP API) and `:50051` (gRPC for workers). pgmq + schema
auto-create on first boot. Note the host's address — workers need it.

## 3. Worker machine(s)

```sh
KATAGO_WS_IMAGE=registry.example.com/katago-ws:latest \
ORCHESTRATOR_HOST=<orchestrator-private-ip> \
docker compose -f worker.compose.yml up -d --scale worker=4
```

Run this on as many hosts as you like; `--scale` adds engines per host. Scale is
elastic — workers connect/disconnect freely, and the orchestrator redistributes.

## 4. Verify

```sh
# from anywhere that can reach the orchestrator's API:
curl http://<orchestrator>:3000/workers          # should list connected workers
curl -X POST --data-binary @game.sgf http://<orchestrator>:3000/analyse
curl "http://<orchestrator>:3000/analyse/<id>?wait=120"
```
Orchestrator logs `worker connected slots=N`; a worker log shows the job; the
result lands in the DB. Kill a worker mid-job → the lease lapses and another
worker picks it up.

## ⚠️ Security — read before exposing :50051

The orchestrator↔worker gRPC is **plaintext h2c with no authentication**. Anyone
who can reach `:50051` can register as a worker, receive submitted SGFs, and
return arbitrary "results" that the orchestrator writes to your DB. So:

- **Keep `:50051` on a private network** — a cloud VPC, or a WireGuard/Tailscale
  mesh between the machines — and **firewall it to the worker hosts only**.
- **Do NOT publish `:50051` to the public internet** as-is.
- The HTTP API (`:3000`) should sit behind a **TLS-terminating reverse proxy**
  (Caddy/nginx/a load balancer) for production; rate-limiting on `POST /analyse`
  is built in (per-IP, in-memory).

There's no TLS/token on the gRPC link yet — if your machines can't share a
private network and you need it over the public internet, that's a feature to
add (mutual TLS + a shared-secret in a gRPC metadata header). Ask and I'll wire it.

## Notes

- **Postgres**: lives with the orchestrator here; swap `MUXA_DIESEL__URL` for a
  managed DB (Neon/RDS/…) if you prefer. Workers still never connect to it.
- **Migrations** run on the orchestrator at startup (workers have no DB).
- **HA**: you can run multiple orchestrators against one DB behind a load
  balancer; each holds its own worker connections + leases (instance-local
  `/workers`), and the shared pgmq queue keeps them coordinated.
