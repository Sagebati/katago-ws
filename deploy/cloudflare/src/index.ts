// Worker entrypoint for the katago-ws Cloudflare Containers deployment.
//
// It owns one container instance (the Rust `orchestrator` role) and forwards all
// HTTP — and WebSocket upgrades — to it. The orchestrator runs the web API +
// Postgres/pgmq + the cluster WebSocket dispatcher (at `/cluster` on port 3000),
// but NO KataGo engine — it hands analysis to remote workers that connect over
// that socket. A Cron Trigger pings /health to keep it awake.
//
// Workers reach the dispatcher at `wss://<this-worker>/cluster`: Container.fetch
// proxies the WS upgrade to the container's port 3000, so no extra port is
// exposed. The socket is authenticated by a shared Bearer token — set the
// MUXA_CLUSTER_TOKEN secret, and the worker presents the same value.

import { Container } from "@cloudflare/containers";

export interface Env {
  KATAGO_WS: DurableObjectNamespace<KatagoWs>;
  // Secrets — set with `wrangler secret put <NAME>`:
  MUXA_DATABASE_URL: string; // postgres://user:pass@host:5432/db  (must be reachable)
  MUXA_SENTRY__DSN?: string;
  MUXA_CLUSTER_TOKEN?: string; // shared secret a worker presents to open /cluster
}

export class KatagoWs extends Container<Env> {
  // The axum server binds 0.0.0.0:3000 (MUXA_WEB__PORT). Requests are held until
  // the container is listening on this port.
  defaultPort = 3000;

  // Stay warm between requests so the background queue worker (and any in-flight
  // KataGo analysis) isn't suspended mid-run. The cron in wrangler.jsonc tops
  // this up during idle periods.
  sleepAfter = "15m";

  // Environment passed into the container (mirrors muxa.toml / Dockerfile ENV).
  // `this.env` (the Worker's vars/secrets) is set by the base constructor before
  // these field initializers run.
  envVars = {
    // Launch role (read by Role::resolve() when no CLI arg is set, which is the
    // case here — the image ENTRYPOINT is just `katago-ws`). Orchestrator = web +
    // pgmq + the cluster WebSocket dispatcher, no in-process engine.
    KATAGO_WS_ROLE: "orchestrator",
    MUXA_WEB__HOST: "0.0.0.0",
    MUXA_WEB__PORT: "3000",
    MUXA_WEB__BANNER: "false",
    // Engine vars are inert in the orchestrator role (no KataGo here) — kept so a
    // flip back to KATAGO_WS_ROLE=standalone needs no other change.
    MUXA_ENGINE__MAX_VISITS: "10",
    MUXA_ENGINE__REQUEST_TIMEOUT_SECS: "600",
    RUST_LOG: "info",
    // The app reads its DB URL from MUXA_DIESEL__URL (the muxa `[diesel]`
    // section). We expose it under the friendlier MUXA_DATABASE_URL secret and
    // pass it through to the container's expected name here.
    MUXA_DIESEL__URL: this.env.MUXA_DATABASE_URL,
    // Cluster auth: the secret workers must present on /cluster. Omitted ⇒ auth
    // disabled (insecure on a public endpoint — set MUXA_CLUSTER_TOKEN!).
    ...(this.env.MUXA_CLUSTER_TOKEN
      ? { MUXA_ORCHESTRATOR__AUTH_TOKEN: this.env.MUXA_CLUSTER_TOKEN }
      : {}),
    ...(this.env.MUXA_SENTRY__DSN
      ? { MUXA_SENTRY__DSN: this.env.MUXA_SENTRY__DSN }
      : {}),
  };

  override onStart() {
    console.log("katago-ws container started");
  }
  override onStop() {
    console.log("katago-ws container stopped");
  }
  override onError(err: unknown) {
    console.error("katago-ws container error:", err);
  }
}

// Single named instance: the orchestrator (cluster WebSocket dispatcher + DB/queue owner). All
// HTTP + the keep-alive pin to it. The name is bumped from "main" because a warm,
// cron-kept-alive instance resumes its OLD image across Cloudflare's gradual
// rollout — changing the Durable Object key forces a fresh instance onto the new
// image/role. (Bump again only if a future deploy gets stuck the same way.)
const INSTANCE = "main-v3";

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    return env.KATAGO_WS.getByName(INSTANCE).fetch(request);
  },

  // Cron keep-alive: a cheap /health hit resets `sleepAfter` so the container
  // stays up and the worker keeps processing pgmq jobs with no user traffic.
  async scheduled(_event: ScheduledController, env: Env): Promise<void> {
    await env.KATAGO_WS.getByName(INSTANCE).fetch(
      new Request("http://container/health"),
    );
  },
} satisfies ExportedHandler<Env>;
