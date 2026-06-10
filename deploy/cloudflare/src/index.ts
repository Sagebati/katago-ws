// Worker entrypoint for the katago-ws Cloudflare Containers deployment.
//
// It owns one container instance (the Rust `standalone` service) and forwards
// all HTTP to it. A Cron Trigger pings /health to keep the container awake so
// the in-process pgmq worker keeps draining the queue between user requests.

import { Container } from "@cloudflare/containers";

export interface Env {
  KATAGO_WS: DurableObjectNamespace<KatagoWs>;
  // Secrets — set with `wrangler secret put <NAME>`:
  MUXA_DATABASE_URL: string; // postgres://user:pass@host:5432/db  (must be reachable)
  MUXA_SENTRY__DSN?: string;
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
    MUXA_WEB__HOST: "0.0.0.0",
    MUXA_WEB__PORT: "3000",
    MUXA_WEB__BANNER: "false",
    // KataGo on <= 4 vCPU is slow — keep visits low so a full game fits the
    // 600s engine timeout. Raise on beefier compute.
    MUXA_ENGINE__MAX_VISITS: "10",
    MUXA_ENGINE__REQUEST_TIMEOUT_SECS: "600",
    RUST_LOG: "info",
    // The app reads its DB URL from MUXA_DIESEL__URL (the muxa `[diesel]`
    // section). We expose it under the friendlier MUXA_DATABASE_URL secret and
    // pass it through to the container's expected name here.
    MUXA_DIESEL__URL: this.env.MUXA_DATABASE_URL,
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

// Single named instance: the `standalone` role runs one in-process queue worker,
// so we pin all traffic + the keep-alive to the same container rather than
// spreading across replicas that each sleep independently. To scale out, switch
// to the orchestrator/worker split (separate deployment), not multiple of these.
const INSTANCE = "main";

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
