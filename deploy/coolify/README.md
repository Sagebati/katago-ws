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

## 6. Sentry (error tracking, optional)
The orchestrator role already loads the Sentry plugin — it just needs a DSN. With
no DSN it runs a no-op client (nothing is sent), so this step is optional.

1. In Sentry: **Create project → Platform: Rust**. Copy its **DSN**
   (`https://<key>@<org>.ingest.sentry.io/<project>`).
2. In Coolify → this resource → **Environment Variables**, add a **secret**:
   - `MUXA_SENTRY__DSN` = the DSN from step 1. That's the only required one.
3. **Redeploy**. On boot the logs show `muxa-sentry: initialized` (vs. `no DSN set,
   running as no-op client`). Panics, errors, and request transactions land in Sentry.

The compose sets `MUXA_ENV=production` (muxa's core run mode), so Sentry's
**environment** defaults to `production` and the **transaction sample rate to
0.1** (10%) — performance monitoring is on, sampled to bound cost/quota. Locally
(debug build ⇒ `development`) both default the other way: environment
`development`, rate **1.0** (every trace). Override per-knob with
`MUXA_SENTRY__ENVIRONMENT` / `MUXA_SENTRY__TRACES_SAMPLE_RATE` (`1.0` = trace
everything, `0.0` = errors only).

What lands in Sentry once the DSN is set:
- **Issues** — panics + `error!` events.
- **Traces / DB spans** — one transaction per HTTP request (sampled at the rate
  above); diesel queries show as child spans. The dashboard (`GET /`) and the
  `/analyses` + `/analyse/{id}` endpoints all hit Postgres, so they populate the
  Performance + DB views. To actually see them, bump
  `MUXA_SENTRY__TRACES_SAMPLE_RATE=1.0` while testing. *(Note: the background
  analysis pipeline runs outside HTTP, so its queries aren't traced yet.)*
- **Logs** — INFO/WARN/ERROR log lines stream to the Logs explorer (muxa enables
  `enable_logs`; set `MUXA_SENTRY__LOGS=false` to turn off).

The DSN is a `SecretString`: it's redacted from `Debug`/logs, so it won't leak into
the dashboard or traces. Workers can take the same `MUXA_SENTRY__DSN` to report
their own KataGo-side errors; tag them apart with `MUXA_SENTRY__ENVIRONMENT`.

## 7. Auto-deploy (optional)
Add the repo secret `COOLIFY_DEPLOY_HOOK` (your Coolify resource's deploy webhook
URL) and `image.yml` will ping it after each push → Coolify pulls + rolls the new
image. Full CD onto your VPS.
