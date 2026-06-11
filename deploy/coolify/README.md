# Orchestrator on Coolify (Postgres on Supabase)

The light **orchestrator** runs on your Coolify VPS (always-on ⇒ no cron). It owns
the public API + the cluster WebSocket; **workers dial in** over `wss://`. Postgres
stays on **Supabase**. KataGo runs only on the workers, never here.

```
  clients ──HTTPS──►  Coolify (Traefik+TLS) ──►  orchestrator :3000  ──►  Supabase Postgres
  workers ──WSS────►        /cluster        ──►  (pushes jobs over the socket)
```

## 1. Image (ghcr.io)
Coolify can't build the Dockerfile (it needs the sibling `muxa`/`pgmq` crates), so
it pulls a prebuilt image. The `image.yml` workflow builds + pushes
`ghcr.io/sagebati/katago-ws:latest` on every push to `main`.

Make sure Coolify can pull it — **one of**:
- Make the **ghcr package public** (GitHub → your profile → Packages → `katago-ws`
  → Package settings → Change visibility → Public), or
- In **Coolify → Settings → Registries**, add `ghcr.io` with a GitHub PAT that has
  `read:packages`, then select it on the resource.

## 2. Supabase connection string
In Supabase: **Project → Connect → Session pooler** (a `…pooler.supabase.com:5432`
URL). Use that — it's IPv4-friendly (your VPS may be IPv4-only) and safe for a
persistent connection pool.
- ✅ Session pooler, port **5432**.
- ❌ Not the transaction pooler (**6543**) — it breaks diesel's prepared statements.
- ❌ Not the direct `db.<ref>.supabase.co` host unless your VPS has IPv6.

The schema + pgmq queue are already created (the previous orchestrator booted
against this same DB), so migrations are a no-op on first connect.

## 3. Deploy in Coolify
1. **New Resource → Docker Compose**, point it at this `deploy/coolify/docker-compose.yml`
   (or paste it).
2. Set environment variables as **secrets**:
   - `MUXA_DIESEL__URL` = the Supabase session-pooler URL from step 2.
   - `MUXA_ORCHESTRATOR__AUTH_TOKEN` = a long random string (see §5).
3. Assign your **domain** (e.g. `orchestrator.abbaye.xyz`, DNS A-record → VPS IP) to
   the `orchestrator` service on port **3000**. Coolify issues the Let's Encrypt
   cert automatically; Traefik proxies WebSockets as-is.
4. Deploy. Check `https://<domain>/health` → `ok`, and `https://<domain>/` shows the
   dashboard (Workers: 0).

## 4. Run a worker (anywhere)
```sh
MUXA_WORKER__ORCHESTRATOR_URL=wss://orchestrator.abbaye.xyz/cluster \
MUXA_WORKER__AUTH_TOKEN=<same token as the orchestrator> \
MUXA_ENGINE__MAX_VISITS=20 \
docker run --rm -e MUXA_WORKER__ORCHESTRATOR_URL -e MUXA_WORKER__AUTH_TOKEN \
  -e MUXA_ENGINE__MAX_VISITS ghcr.io/sagebati/katago-ws:latest worker
```
The dashboard's **Workers** count goes 0 → 1. Submit a game:
`curl -X POST --data-binary @game.sgf https://orchestrator.abbaye.xyz/analyse`.

> Keep `MUXA_ENGINE__MAX_VISITS` modest (8–50) so a full game finishes inside the
> orchestrator's lease window — high visits are what made earlier jobs time out.

## 5. ⚠️ Auth — this socket is public
`wss://orchestrator.abbaye.xyz/cluster` is reachable by anyone. With an empty token,
**any stranger can register as a worker** (receive your SGFs, return junk results).
Set `MUXA_ORCHESTRATOR__AUTH_TOKEN` and give workers the matching
`MUXA_WORKER__AUTH_TOKEN`. For an open pool, move to per-worker tokens later so a
bad worker can be revoked without rotating everyone's.

## 6. Auto-deploy (optional)
Add the repo secret `COOLIFY_DEPLOY_HOOK` (your Coolify resource's deploy webhook
URL) and `image.yml` will ping it after each push → Coolify pulls + rolls the new
image. Full CD onto your VPS.
