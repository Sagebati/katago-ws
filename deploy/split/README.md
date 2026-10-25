# Split deployment: orchestrator + workers on separate machines

The `orchestrator` owns Postgres + the queue and dispatches analysis to remote
`worker` machines over a WebSocket (`/cluster`, on the orchestrator's web port).
Workers run the engine (katago-rs) and **never touch Postgres** — they only dial
the orchestrator.
This is the topology the cluster split was built for.

```
        HTTP + WS :3000               ws(s)://…:3000/cluster (Bearer auth)
  clients ───────────►  ORCHESTRATOR  ◄──────────────────────  WORKER host A (xN engines)
  (POST /analyse)       + Postgres     ◄──────────────────────  WORKER host B
                        (lease/redelivery stays here)           WORKER host C …
```

## 1. Build the image once, push to a registry

CI publishes the images to `ghcr.io/sagebati/katago-ws` (`:cpu`, `:vulkan`,
`:rocm`, and `:latest` = `:cpu`, on every push to `main`). To build your own, the
Dockerfile needs a **parent** build context (it COPYs the sibling `muxa`, `pgmq`
and `katago-rs` checkouts, at the refs in `.github/sibling-refs.env`), so build
from the directory that contains them:

```sh
cd /path/to/rust            # contains katago-ws/, muxa/, pgmq/, katago-rs/
docker build --platform linux/amd64 -f katago-ws/Dockerfile \
  -t registry.example.com/katago-ws:latest .
docker push registry.example.com/katago-ws:latest
```

One image serves every role — the role is just the first CLI arg (the compose
`command:`). Use that `KATAGO_WS_IMAGE` value on all machines.

### Picking a worker image

The plain build above is the CPU image. GPU workers want a per-platform variant,
built from `katago-ws/` with `just cpu | vulkan | rocm` (the justfile has the
build-args). All three run katago-rs, KataGo's analysis engine in Rust, with its
nets compiled in:

| Image    | Backend | Net (+ human SL)                | Use it for | Size |
|----------|---------|---------------------------------|------------|-----:|
| `cpu`    | flex (CPU)          | b10c128                        | the orchestrator; CPU-only workers | 121 MB |
| `vulkan` | wgpu (Vulkan), f16  | b18c384nbt (+ b18c384nbt-humanv0) | NVIDIA, Intel, and AMD GPUs ROCm doesn't support | VULKAN_SIZE |
| `rocm`   | HIP, f16            | b18c384nbt (+ b18c384nbt-humanv0) | **AMD GPUs that ROCm supports** (RDNA2+/CDNA) | ROCM_SIZE |

The GPU images ship b10c128 too (`KATAGO_WS_ENGINE__MODEL=/opt/katago-rs/nets/g170e-b10c128.bpk`).
On an RX 6800 XT, `rocm` with b18c384nbt at 100 visits does ~13.4 positions/s
(stock KataGo 1.16.5's OpenCL backend: 7.1).

`rocm` is Ubuntu 24.04 with AMD's HIP runtime 7.2.3 from repo.radeon.com. It
needs the KFD and DRM devices and the host's `video`/`render` GIDs (the image runs
as uid 10001); the container's ROCm needn't match the host's, only the kernel's
amdgpu driver:

```sh
docker run --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group video | cut -d: -f3)" \
  --group-add "$(getent group render | cut -d: -f3)" \
  -e KATAGO_WS_WORKER__ORCHESTRATOR_URL=ws://<orchestrator>:3000/cluster \
  -v katago-tuning:/home/app/.katago \
  katago-ws:rocm worker
```

`vulkan` needs `--device /dev/dri` and the `render` GID on AMD and Intel (Mesa's
RADV / ANV); on NVIDIA, the NVIDIA Container Toolkit with `--gpus all` (the image
asks for the `graphics` driver capability, which carries the Vulkan driver).

Run one worker per GPU. The first start compiles the GPU kernels and auto-tunes
the throughput settings (cached in `/home/app/.katago`: mount a volume there to
keep the tuning across restarts).

## 2. Orchestrator machine

```sh
KATAGO_WS_IMAGE=registry.example.com/katago-ws:latest \
POSTGRES_PASSWORD=$(openssl rand -hex 16) \
CLUSTER_TOKEN=$(openssl rand -hex 32) \
docker compose -f orchestrator.compose.yml up -d
```

Exposes `:3000` — the HTTP API **and** the `/cluster` worker WebSocket on the same
port. pgmq + schema auto-create on first boot. Note the host's address and the
`CLUSTER_TOKEN` value — workers need both.

## 3. Worker machine(s)

```sh
KATAGO_WS_IMAGE=registry.example.com/katago-ws:latest \
ORCHESTRATOR_HOST=<orchestrator-private-ip> \
CLUSTER_TOKEN=<same secret the orchestrator was given> \
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
Orchestrator logs `worker connected (ws)` with the worker's name + slot count; a
worker log shows the job; the result lands in the DB. Kill a worker mid-job → the
lease lapses and another worker picks it up.

## ⚠️ Security — read before exposing the cluster socket

The cluster WebSocket lives at `/cluster` on the orchestrator's web port
(`:3000`) and is authenticated by a **shared Bearer token**: `CLUSTER_TOKEN` sets
the orchestrator's `KATAGO_WS_ORCHESTRATOR__AUTH_TOKEN`, and each worker presents the
same value as `KATAGO_WS_WORKER__AUTH_TOKEN`. A connection without the right token is
rejected with `401` before it can register. **Always set a strong `CLUSTER_TOKEN`**
— if it's empty, auth is disabled and anyone who can reach `/cluster` can register
as a worker, receive submitted SGFs, and write arbitrary "results" to your DB.

The token gates access, but plain `ws://` is still **cleartext** — the token and
the SGFs travel unencrypted. So:

- **Use TLS for any untrusted path.** Front `:3000` with a TLS-terminating reverse
  proxy (Caddy/nginx/a load balancer) and have workers dial `wss://<host>/cluster`
  (`KATAGO_WS_WORKER__ORCHESTRATOR_URL`). One cert then protects both the public HTTP
  API and the cluster socket. Rate-limiting on `POST /analyse` is built in (per-IP,
  in-memory).
- **Or keep workers on a private network** — a cloud VPC, or a WireGuard/Tailscale
  mesh — if you'd rather not terminate TLS. The token still guards against stray
  connections within it.
- Either way, **never expose an unauthenticated (`CLUSTER_TOKEN` empty) socket** to
  the public internet.

## Notes

- **Postgres**: lives with the orchestrator here; swap `KATAGO_WS_DIESEL__URL` for a
  managed DB (Neon/RDS/…) if you prefer. Workers still never connect to it.
- **Migrations** run on the orchestrator at startup (workers have no DB).
- **HA**: you can run multiple orchestrators against one DB behind a load
  balancer; each holds its own worker connections + leases (instance-local
  `/workers`), and the shared pgmq queue keeps them coordinated.
