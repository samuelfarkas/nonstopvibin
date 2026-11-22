// A profile without an OAuth account runs its core only while something uses it: the first
// gateway request starts it, and it stops once idle. OAuth profiles keep theirs running so
// the core can refresh their tokens.
import { afterAll, beforeAll } from "bun:test";
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { httpManagement, type Management } from "../src/server/core.ts";
import { usageTiming, type UsageTiming } from "../src/server/usage.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import type { Application } from "../src/server/server.ts";
import type { Profile } from "../src/shared/types.ts";
import { usageSummary } from "./support/usage-summary.ts";
import {
  closeApplication,
  closeServer,
  customAccount,
  listen,
  management,
  signedIn,
  testApplication,
} from "./support/application.ts";

let upstream: http.Server;
let upstreamBase: string;
/** The upstream answers requests for this model 429 with a 1 s Retry-After. */
let limited: string | undefined;
beforeAll(async () => {
  upstream = http.createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const { model } = JSON.parse(Buffer.concat(chunks).toString() || "{}");
    if (model === limited) {
      res.writeHead(429, {
        "Content-Type": "application/json",
        "Retry-After": "1",
      });
      res.end(
        JSON.stringify({
          error: { message: "quota exhausted", type: "rate_limit_error" },
        }),
      );
      return;
    }
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        id: "fixture",
        object: "chat.completion",
        created: 1,
        model: "fixture-model",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "hello" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 11, completion_tokens: 3, total_tokens: 14 },
      }),
    );
  });
  upstreamBase = `http://127.0.0.1:${await listen(upstream)}/v1`;
}, 10_000);
afterAll(() => closeServer(upstream), 10_000);

/** Short scheduler periods; `idle` is how long an unused lazy core keeps running. */
const timing = (idle: number): UsageTiming => ({
  ...usageTiming,
  coalesce: 20,
  activity: 100,
  idle,
});

/**
 * An app whose one enabled profile, from an earlier run, has an API-key account; `oauth`
 * gives it a Claude subscription too, as an earlier account sync would have left it.
 */
async function lazyApp(
  idle: number,
  options: { oauth?: boolean; management?: Management } = {},
): Promise<{
  app: Application;
  profile: Profile;
  remove: () => Promise<void>;
}> {
  const directory = await mkdtemp(join(tmpdir(), "nv-lazy-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const profile = store.createProfile("Lazy fixture");
  store.saveProfile({ ...profile, enabled: true });
  store.saveApiAccount(
    profile.id,
    customAccount("lazy-key", upstreamBase, [
      { id: "fixture-model", protocol: "openai" },
      { id: "other-model", protocol: "openai" },
    ]),
    "lazy-key",
  );
  if (options.oauth) signedIn(store, profile.id);
  store.close();
  const app = await testApplication(directory, {
    cores: { usageTiming: timing(idle), management: options.management },
  });
  return {
    app,
    profile,
    remove: async () => {
      await closeApplication(app);
      await rm(directory, { recursive: true, force: true });
    },
  };
}

function chat(
  app: Application,
  profile: Profile,
  model = "fixture-model",
): Promise<Response> {
  return fetch(`${app.endpoint(profile.id)}/chat/completions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${app.store.clientKey(profile.id)}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      model,
      messages: [{ role: "user", content: "hello" }],
    }),
  });
}

/** A Responses WebSocket handshake through the gateway; resolves with the gateway's answer. */
function handshake(
  app: Application,
  profile: Profile,
): Promise<http.IncomingMessage> {
  return new Promise((resolve, reject) => {
    const req = http.get(`${app.endpoint(profile.id)}/responses`, {
      headers: {
        Authorization: `Bearer ${app.store.clientKey(profile.id)}`,
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
        "Sec-WebSocket-Version": "13",
      },
    });
    req.once("upgrade", (upgraded, socket) => {
      socket.destroy();
      resolve(upgraded);
    });
    req.once("response", (answered) => {
      answered.resume();
      resolve(answered);
    });
    req.once("error", reject);
    req.setTimeout(10_000, () =>
      req.destroy(new Error("WebSocket handshake timed out")),
    );
  });
}

async function until(what: string, condition: () => boolean, ms = 5000) {
  const end = Date.now() + ms;
  while (!condition()) {
    if (Date.now() > end) throw new Error(`Timed out waiting for ${what}`);
    await delay(20);
  }
}

test(
  "a profile without an OAuth account starts its core on its first request",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(600_000);
    try {
      assert.equal(app.cores.pool.alive(profile.id), false, "not at launch");
      assert.deepEqual(
        app.status().running,
        [profile.name],
        "the tray still lists it: it serves",
      );
      const response = await chat(app, profile);
      assert.equal(response.status, 200, await response.text());
      assert.equal(app.cores.pool.running(profile.id), true);
    } finally {
      await remove();
    }
  },
);

test(
  "a dormant profile's first WebSocket starts its core",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(600_000);
    try {
      assert.equal(app.cores.pool.alive(profile.id), false, "not at launch");
      const response = await handshake(app, profile);
      assert.equal(response.statusCode, 101);
      assert.equal(response.headers["x-nonstopvibin-profile"], profile.slug);
      assert.equal(app.cores.pool.running(profile.id), true);
    } finally {
      await remove();
    }
  },
);

test(
  "requests that arrive together at a dormant profile all wait for its one core",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(600_000);
    try {
      const [first, second, third, socket] = await Promise.all([
        chat(app, profile),
        chat(app, profile),
        chat(app, profile),
        handshake(app, profile),
      ]);
      for (const response of [first, second, third])
        assert.equal(response.status, 200, await response.text());
      assert.equal(socket.statusCode, 101);
    } finally {
      await remove();
    }
  },
);

test(
  "only a UI's state polls start dormant cores; the CLI's state read does not",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(600_000);
    try {
      assert.equal((await management(app, "/state")).status, 200);
      assert.equal(app.cores.pool.alive(profile.id), false, "the CLI's read");
      assert.equal((await management(app, "/state?watch=1")).status, 200);
      assert.equal(app.cores.pool.running(profile.id), true, "a UI's poll");
    } finally {
      await remove();
    }
  },
);

test(
  "the model catalog starts a dormant core",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(600_000);
    try {
      const response = await management(app, `/profiles/${profile.id}/models`);
      const body = await response.text();
      assert.equal(response.status, 200, body);
      assert.match(body, /"id":"fixture-model"/);
    } finally {
      await remove();
    }
  },
);

test(
  "an idle core stops after its last request without losing its usage, and the next request starts it again",
  { timeout: 30_000 },
  async () => {
    // Reads of the queue find it empty until shortly before the idle stop, so only the
    // stop's own drains can collect what the core still holds.
    let hidden = true;
    const { app, profile, remove } = await lazyApp(400, {
      management: async (request) =>
        hidden && request.path.startsWith("/usage-queue")
          ? []
          : httpManagement(request),
    });
    try {
      for (let i = 0; i < 3; i++)
        assert.equal((await chat(app, profile)).status, 200);
      await delay(250);
      assert.equal(usageSummary(app.store, profile.id).requests, 0);
      hidden = false;
      await until("the idle stop", () => !app.cores.pool.alive(profile.id));
      assert.equal(
        usageSummary(app.store, profile.id).requests,
        3,
        "every request was collected before the core stopped",
      );
      assert.equal(app.cores.usage.isDormant(profile.id), true);
      assert.deepEqual(app.status().running, [profile.name]);
      await delay(300);
      assert.equal(
        app.cores.pool.alive(profile.id),
        false,
        "the restart policy does not start it again",
      );

      assert.equal((await chat(app, profile)).status, 200);
      await until(
        "the next request's usage",
        () => usageSummary(app.store, profile.id).requests === 4,
      );
    } finally {
      await remove();
    }
  },
);

test(
  "a request that arrives while an idle stop drains its core is served by the next core, and nothing is lost",
  { timeout: 30_000 },
  async () => {
    let release!: () => void;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    let stopping = (_profileId: string) => false;
    const { app, profile, remove } = await lazyApp(400, {
      // The stop's own final drain, made while its core is stopping, waits for the request below.
      management: async (request) => {
        if (
          request.path.startsWith("/usage-queue") &&
          stopping(request.profileId)
        )
          await held;
        return httpManagement(request);
      },
    });
    stopping = (profileId) => app.cores.pool.holding(profileId);
    try {
      assert.equal((await chat(app, profile)).status, 200);
      const first = app.cores.pool.runtimes.get(profile.id);
      await until("the stop to begin", () =>
        app.cores.pool.holding(profile.id),
      );
      const late = chat(app, profile);
      await delay(100);
      release();
      const response = await late;
      assert.equal(response.status, 200, await response.text());
      const next = app.cores.pool.runtimes.get(profile.id);
      assert.notEqual(next, first, "a new core served it");
      assert.equal(app.cores.pool.running(profile.id), true);
      await until(
        "both requests' usage",
        () => usageSummary(app.store, profile.id).requests === 2,
      );
    } finally {
      release();
      await remove();
    }
  },
);

test(
  "an idle stop that cannot read the usage queue keeps the core running",
  { timeout: 30_000 },
  async () => {
    let failing = false;
    const { app, profile, remove } = await lazyApp(300, {
      management: async (request) => {
        if (failing && request.path.startsWith("/usage-queue"))
          throw new Error("The fixture queue is unreadable.");
        return httpManagement(request);
      },
    });
    try {
      assert.equal((await chat(app, profile)).status, 200);
      await until(
        "the usage",
        () => usageSummary(app.store, profile.id).requests === 1,
      );
      failing = true;
      await until("a reported failed read", () =>
        app
          .state()
          .errors.some((error) => error.includes("Usage before an idle stop")),
      );
      assert.equal(app.cores.pool.running(profile.id), true);
      assert.equal(app.cores.usage.isDormant(profile.id), false);
    } finally {
      failing = false;
      await remove();
    }
  },
);

test(
  "an idle core whose key was rate-limited keeps running until a request on it succeeds",
  { timeout: 60_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(300);
    try {
      limited = "fixture-model";
      assert.equal((await chat(app, profile)).status, 429);
      await until(
        "the 429's usage",
        () => usageSummary(app.store, profile.id).failed >= 1,
      );
      await delay(1000);
      assert.equal(
        app.cores.pool.running(profile.id),
        true,
        "a new core would forget the cooldown and send the next request to the exhausted key",
      );

      limited = undefined;
      // The core holds its own cooldown (about 10 s) before trying the key again.
      let status = 0;
      for (let i = 0; i < 30 && status !== 200; i++) {
        status = (await chat(app, profile)).status;
        if (status !== 200) await delay(1000);
      }
      assert.equal(status, 200);
      await until("the idle stop", () => !app.cores.pool.alive(profile.id));
    } finally {
      limited = undefined;
      await remove();
    }
  },
);

test(
  "a success on another model of the same key does not end a model's cooldown",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(300);
    try {
      limited = "fixture-model";
      assert.equal((await chat(app, profile)).status, 429);
      assert.equal((await chat(app, profile, "other-model")).status, 200);
      await until(
        "both outcomes",
        () => usageSummary(app.store, profile.id).requests >= 2,
      );
      await delay(1000);
      assert.equal(
        app.cores.pool.running(profile.id),
        true,
        "fixture-model is still cooling down in this core",
      );
    } finally {
      limited = undefined;
      await remove();
    }
  },
);

test(
  "a profile with an OAuth account starts at launch and never stops for being idle",
  { timeout: 30_000 },
  async () => {
    const { app, profile, remove } = await lazyApp(200, {
      oauth: true,
      // Its core lists the Claude subscription the earlier run synced.
      management: async (request) =>
        request.path === "/auth-files"
          ? {
              files: [
                { name: "claude.json", provider: "claude", auth_index: "1" },
              ],
            }
          : httpManagement(request),
    });
    try {
      assert.equal(app.cores.pool.running(profile.id), true, "at launch");
      assert.equal((await chat(app, profile)).status, 200);
      await delay(800);
      assert.equal(
        app.cores.pool.running(profile.id),
        true,
        "still running after four idle periods",
      );
    } finally {
      await remove();
    }
  },
);
