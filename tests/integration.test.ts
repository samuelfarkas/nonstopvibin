// node:test hooks ignore their timeout option under Bun (5 s default); these start and stop cores.
import { afterAll, beforeAll } from "bun:test";
import { test } from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SettingsPage } from "../src/client/Settings.tsx";
import coreRelease from "../src/server/core-release.json";
import http from "node:http";
import { connect, type Socket } from "node:net";
import { once } from "node:events";
import {
  mkdtemp,
  readFile,
  readdir,
  stat,
  rm,
  mkdir,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { ModelCatalog } from "../src/server/model-catalog.ts";
import { agentDefinitions } from "../src/server/agents.ts";
import { Application } from "../src/server/server.ts";
import {
  idleCore,
  installFakeCore,
  listeningCore,
} from "./support/fake-core.ts";
import { httpManagement, type Management } from "../src/server/core.ts";
import {
  coreConfigPath,
  credentialDirectory,
  profileDirectory,
} from "../src/server/data-directory.ts";
import { Cores } from "../src/server/cores.ts";
import { usageTiming } from "../src/server/usage.ts";
import { allHistoryDays, usagePageSize } from "../src/shared/vocabulary.ts";
import { Store, secretIds } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { usageSummary } from "./support/usage-summary.ts";
import type { Json, Profile } from "../src/shared/types.ts";
import { record } from "../src/server/json.ts";
import {
  closeApplication,
  closeServer,
  customAccount,
  listen,
  management,
  testApplication,
} from "./support/application.ts";

let app: Application;
let company: Profile;
let personal: Profile;
// Running profiles that each belong to one test, so no other test changes what it counts.
let accounting: Profile;
let removal: Profile;
let finalDrain: Profile;
let directory: string;
let failCompany = false;
let onHang: (() => void) | undefined;
const observed: string[] = [];
// The shared app's management calls go through this when a test sets it.
let intercept: Management | undefined;
// The shared app's scheduler periods; a test may shorten one for its duration.
const appUsageTiming = { ...usageTiming };
let upstream: http.Server;
let upstreamBase: string;
async function request(
  profile: Profile,
  body: Json = {
    model: "fixture-model",
    messages: [{ role: "user", content: "hello" }],
  },
) {
  return fetch(`${app.endpoint(profile.id)}/chat/completions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${app.store.clientKey(profile.id)}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(20_000),
  });
}
beforeAll(async () => {
  directory = await mkdtemp(join(tmpdir(), "nonstopvibin-test-"));
  upstream = http.createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const key = req.headers.authorization?.replace("Bearer ", "") ?? "";
    observed.push(key);
    if (failCompany && key.startsWith("company")) {
      res.writeHead(429, {
        "Content-Type": "application/json",
        "Retry-After": "60",
      });
      res.end(
        JSON.stringify({
          error: { message: "quota exhausted", type: "rate_limit_error" },
        }),
      );
      return;
    }
    const body = JSON.parse(Buffer.concat(chunks).toString() || "{}");
    if (body.messages?.[0]?.content === "hang") {
      // Never answer: the caller must give up before response headers arrive.
      onHang?.();
      return;
    }
    if (body.stream) {
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.write(
        `data: ${JSON.stringify({ id: "fixture", object: "chat.completion.chunk", created: 1, model: "fixture-model", choices: [{ index: 0, delta: { role: "assistant", content: `hello from ${key}` }, finish_reason: null }] })}\n\n`,
      );
      res.end(
        `data: ${JSON.stringify({ id: "fixture", object: "chat.completion.chunk", created: 1, model: "fixture-model", choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 11, completion_tokens: 3, total_tokens: 14, prompt_tokens_details: { cached_tokens: 5 } } })}\n\ndata: [DONE]\n\n`,
      );
    } else {
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
              message: { role: "assistant", content: `hello from ${key}` },
              finish_reason: "stop",
            },
          ],
          usage: {
            prompt_tokens: 11,
            completion_tokens: 3,
            total_tokens: 14,
            prompt_tokens_details: { cached_tokens: 5 },
          },
        }),
      );
    }
  });
  upstreamBase = `http://127.0.0.1:${await listen(upstream)}/v1`;
  app = await testApplication(directory, {
    cores: {
      management: (request) => (intercept ?? httpManagement)(request),
      usageTiming: appUsageTiming,
    },
  });
  company = app.store.createProfile("Company A");
  personal = app.store.createProfile("Personal");
  accounting = app.store.createProfile("Accounting fixture");
  removal = app.store.createProfile("Removal fixture");
  finalDrain = app.store.createProfile("Final drain fixture");
  for (const [profile, keys] of [
    [company, ["company-one", "company-two"]],
    [personal, ["personal-one"]],
    [accounting, ["accounting-one"]],
    [removal, ["removal-one"]],
  ] as const)
    for (const key of keys)
      app.store.saveApiAccount(
        profile.id,
        customAccount(key, upstreamBase, [
          { id: "fixture-model", protocol: "openai" },
        ]),
        key,
      );
  await Promise.all(
    [company, personal, accounting, removal, finalDrain].map((profile) =>
      app.cores.pool.start(profile.id),
    ),
  );
}, 30_000);
/** Sets a profile's routing for the duration of `body`, then restores what it was. */
async function withRouting(
  profile: Profile,
  routing: Pick<Profile, "strategy" | "sessionAffinity">,
  body: () => Promise<void>,
) {
  const patch = async (value: typeof routing) => {
    const changed = await management(app, `/profiles/${profile.id}`, {
      method: "PATCH",
      body: value,
    });
    assert.equal(changed.status, 200, await changed.text());
  };
  const { strategy, sessionAffinity } = app.store.profile(profile.id);
  await patch(routing);
  try {
    await body();
  } finally {
    await patch({ strategy, sessionAffinity });
  }
}
/** Management answers as `listeningCore` gives them, for a core that does not listen. */
const fakeManagement: Management = async ({ path }) =>
  path === "/auth-files"
    ? { files: [] }
    : path.startsWith("/usage-queue")
      ? []
      : {};
/**
 * An application whose profile core is `source`, a script run as the core binary with the
 * core's arguments; `management` answers in place of the core's management API if given.
 */
async function fakeCoreApplication(
  name: string,
  source: string,
  management?: Management,
) {
  const root = join(directory, name);
  const binary = await installFakeCore(root, source);
  const instance = await testApplication(join(root, "data"), {
    binary,
    cores: { management },
  });
  const profile = instance.store.createProfile("Fake core");
  try {
    await instance.cores.pool.start(profile.id);
  } catch (error) {
    await instance.close();
    throw error;
  }
  return { instance, profile, close: () => closeApplication(instance) };
}
// Answers a WebSocket handshake with browser-facing headers.
const misbehavingCore = listeningCore(
  undefined,
  `server.on("upgrade", (_req, socket) => socket.end("HTTP/1.1 101 Switching Protocols\\r\\nUpgrade: websocket\\r\\nConnection: Upgrade\\r\\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\\r\\nSet-Cookie: a=1\\r\\nSet-Cookie: b=2\\r\\nAccess-Control-Allow-Origin: *\\r\\nX-NonstopVibin-Profile: forged\\r\\n\\r\\n"));`,
);
afterAll(async () => {
  if (app) await closeApplication(app);
  if (upstream) await closeServer(upstream);
  if (directory) await rm(directory, { recursive: true, force: true });
}, 30_000);
test("a malformed static path is not found and not reported as an app error", async () => {
  const reported = app.state().errors.length;
  const response = await fetch(`${app.origin}/%E0`);
  assert.equal(response.status, 404);
  await response.text();
  assert.equal(app.state().errors.length, reported);
});
test("a malformed API path parameter is rejected and not reported as an app error", async () => {
  const reported = app.state().errors.length;
  const response = await management(
    app,
    `/profiles/${company.id}/accounts/%E0`,
    { method: "DELETE" },
  );
  assert.equal(response.status, 400);
  await response.text();
  assert.equal(app.state().errors.length, reported);
});
test("a management client that aborts mid-body is not reported as an app error", async () => {
  const reported = app.state().errors.length;
  const { host, port } = new URL(app.origin);
  const socket = connect(Number(port), "127.0.0.1");
  await once(socket, "connect");
  socket.write(
    `POST /api/profiles HTTP/1.1\r\nHost: ${host}\r\nAuthorization: Bearer ${app.token}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{"name":`,
  );
  await delay(100);
  socket.destroy();
  await delay(200);
  assert.equal(app.state().errors.length, reported);
});
test("a rotated or deleted profile key is rejected on the very next request", async () => {
  const profile = app.store.createProfile("Key rotation fixture");
  const bystander = app.store.createProfile("Key bystander");
  const status = async (key: string) => {
    const response = await fetch(`${app.origin}/v1/models`, {
      headers: { Authorization: `Bearer ${key}` },
    });
    await response.text();
    return response.status;
  };
  // Accepted keys reach the stopped-profile check; rejected keys never do.
  // Stopped profiles keep this off the running cores, whose activity later tests time.
  const original = app.store.clientKey(profile.id);
  assert.equal(await status(original), 503);
  const rotated = `nv_${"R".repeat(43)}`;
  app.store.setSecret(secretIds.client(profile.id), rotated);
  assert.equal(await status(original), 401);
  assert.equal(await status(rotated), 503);
  // Removal outside the Store's write methods must fail closed too.
  app.store.db.prepare("DELETE FROM profiles WHERE id=?").run(profile.id);
  assert.equal(await status(rotated), 401);
  app.store.db
    .prepare("DELETE FROM secrets WHERE id LIKE ?")
    .run(`${profile.id}:%`);
  assert.equal(await status(rotated), 401);
  assert.equal(await status(app.store.clientKey(bystander.id)), 503);
});
test("static assets revalidate without caching management responses", async () => {
  const client = join(directory, "static-fixture");
  await mkdir(join(client, "assets"), { recursive: true });
  await writeFile(join(client, "index.html"), "fixture page");
  await writeFile(join(client, "assets/app-AbCd1234.js"), "fixture script");
  const served = await Application.create({
    ...app.options,
    directory: join(directory, "static-app"),
    clientDirectory: client,
    port: 0,
  });
  try {
    const first = await fetch(served.origin);
    assert.equal(await first.text(), "fixture page");
    assert.equal(first.headers.get("cache-control"), "no-cache");
    const etag = first.headers.get("etag")!;
    const restarted = await Application.create({
      ...served.options,
      directory: join(directory, "restart-fixture"),
      port: 0,
    });
    try {
      const fresh = await fetch(restarted.origin, {
        headers: { "If-None-Match": etag },
      });
      assert.equal(
        fresh.status,
        200,
        "unhashed assets revalidate across launches even with identical timestamps",
      );
      assert.notEqual(fresh.headers.get("etag"), etag);
    } finally {
      await restarted.close();
    }
    for (const match of [etag, etag.slice(2), `"other", ${etag}`, "*"]) {
      const cached = await fetch(served.origin, {
        headers: { "If-None-Match": match },
      });
      assert.equal(cached.status, 304);
      assert.equal(await cached.text(), "");
      assert.equal(cached.headers.get("etag"), etag);
      assert.ok(cached.headers.has("content-security-policy"));
    }
    await writeFile(join(client, "index.html"), "updated fixture page");
    const updated = await fetch(served.origin, {
      headers: { "If-None-Match": etag },
    });
    assert.equal(updated.status, 200);
    assert.equal(await updated.text(), "updated fixture page");
    const asset = await fetch(`${served.origin}/assets/app-AbCd1234.js`);
    assert.equal(await asset.text(), "fixture script");
    assert.equal(
      asset.headers.get("cache-control"),
      "public, max-age=31536000, immutable",
    );
    const head = await fetch(served.origin, { method: "HEAD" });
    assert.equal(head.status, 200);
    assert.equal(await head.text(), "");
    const state = await fetch(`${served.origin}/api/state`, {
      headers: {
        Authorization: `Bearer ${served.token}`,
        "If-None-Match": "*",
      },
    });
    assert.equal(state.status, 200);
    assert.equal(state.headers.get("cache-control"), "no-store");
    const missing = await fetch(`${served.origin}/missing.js`, {
      headers: { "If-None-Match": "*" },
    });
    assert.equal(missing.status, 404);
  } finally {
    await served.close();
  }
});
test("settings displays the core release pin supplied by application state", () => {
  const state = app.state();
  assert.equal(state.coreVersion, coreRelease.version);
  const html = renderToStaticMarkup(
    createElement(SettingsPage, {
      state,
      refresh: async () => {},
      onError: assert.fail,
    }),
  );
  assert.ok(html.includes(`<code>${coreRelease.version}</code>`));
  assert.match(html, /CLIProxyAPI/);
});

test("real core launches with separate owner-only directories and a model catalog", async () => {
  const config = record(await app.cores.pool.management(company.id, "/config"));
  assert.equal(config["commercial-mode"], true);
  assert.equal(config["usage-statistics-enabled"], true);
  assert.notEqual(
    app.cores.pool.port(company.id),
    app.cores.pool.port(personal.id),
  );
  assert.ok(
    (await app.cores.pool.models(company.id)).some(
      (m) => m.id === "fixture-model",
    ),
  );
  for (const profile of [company, personal]) {
    const config = coreConfigPath(app.store.directory, profile.id);
    assert.equal((await stat(config)).mode & 0o777, 0o600);
    assert.equal(
      (await stat(credentialDirectory(app.store.directory, profile.id))).mode &
        0o777,
      0o700,
    );
  }
  const raw = await readFile(join(directory, "nonstopvibin.sqlite"));
  assert.equal(
    raw.includes(Buffer.from(app.store.clientKey(company.id))),
    false,
  );
});

test("usage retention management rejects profile keys and invalid limits", async () => {
  const put = (token: string, days: number) =>
    management(app, "/usage-retention", {
      method: "PUT",
      body: { days },
      token,
    });
  assert.equal((await put(app.store.clientKey(company.id), 30)).status, 401);
  assert.equal((await put(app.token, 1)).status, 400);
  assert.equal((await put(app.token, 0)).status, 200);
  assert.equal(app.store.usageRetentionDays(), 0);
});

test(
  "slow account maintenance does not block usage collection",
  { timeout: 10_000 },
  async () => {
    // An isolated pool with short intervals; the shared app keeps production timing.
    const root = join(directory, "maintenance-fixture");
    await mkdir(root, { mode: 0o700 });
    const store = new Store(root, fileKeyCodec(root));
    let enter!: () => void;
    let resume!: () => void;
    const entered = new Promise<void>((resolve) => {
      enter = resolve;
    });
    const release = new Promise<void>((resolve) => {
      resume = resolve;
    });
    let started = false;
    let collections = 0;
    let collected = () => {};
    const cores = new Cores(store, app.options.binary, {
      report: () => {},
      usageTiming: { ...usageTiming, accounts: 200, coalesce: 20 },
      management: async (request) => {
        if (started && request.path === "/auth-files") {
          enter();
          await release;
        }
        if (started && request.path.startsWith("/usage-queue")) {
          collections++;
          collected();
        }
        return httpManagement(request);
      },
    });
    const profile = store.createProfile("Maintenance fixture");
    try {
      await cores.pool.start(profile.id);
      started = true;
      cores.usage.begin();
      await entered;
      assert.ok(
        collections <= 2,
        "idle profiles do not authenticate every two seconds",
      );
      const before = collections;
      const next = new Promise<void>((resolve) => {
        collected = resolve;
      });
      cores.usage.markActivity(profile.id);
      await next;
      assert.ok(
        collections > before,
        "accounting continues while maintenance is waiting",
      );
    } finally {
      resume();
      await cores.shutdown();
      store.close();
    }
  },
);
test("concurrent usage drains share one destructive queue read", async () => {
  // Nothing reads this queue between this drain ending and the two below starting.
  await app.cores.usage.collect(company.id);
  let reads = 0;
  intercept = (request) => {
    if (
      request.profileId === company.id &&
      request.path.startsWith("/usage-queue")
    )
      reads++;
    return httpManagement(request);
  };
  try {
    await Promise.all([
      app.cores.usage.collect(company.id),
      app.cores.usage.collect(company.id),
    ]);
    assert.equal(reads, 1);
  } finally {
    intercept = undefined;
  }
});
test("verified CLIProxyAPI exposes its native Codex catalog through the same profile", async () => {
  const response = await fetch(
    `${app.endpoint(company.id)}/models?client_version=0.140.0`,
    {
      headers: {
        Authorization: `Bearer ${app.store.clientKey(company.id)}`,
      },
    },
  );
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.ok(
    Array.isArray(body.models),
    "the core must supply Codex's models[] format",
  );
  assert.deepEqual(
    body.models.map((model: { slug: string }) => model.slug),
    ["fixture-model"],
  );
  assert.equal(body.models[0].visibility, "list");
  assert.match(body.models[0].base_instructions, /./);
});

test("management requires its own token and rejects foreign origins/hosts", async () => {
  assert.equal((await fetch(`${app.origin}/api/state`)).status, 401);
  assert.equal(
    (
      await fetch(`${app.origin}/api/state`, {
        headers: {
          Authorization: `Bearer ${app.token}`,
          Origin: "https://evil.example",
        },
      })
    ).status,
    403,
  );
  // Node fetch owns the Host header; use raw HTTP to actually put a foreign Host on the wire.
  const foreignHostStatus = await new Promise<number | undefined>(
    (resolve, reject) => {
      const request = http.get(
        `${app.origin}/api/state`,
        {
          headers: {
            Authorization: `Bearer ${app.token}`,
            Host: "evil.example",
          },
        },
        (response) => {
          response.resume();
          resolve(response.statusCode);
        },
      );
      request.once("error", reject);
    },
  );
  assert.equal(foreignHostStatus, 403);
  assert.equal(
    (
      await fetch(`${app.origin}/api/state`, {
        headers: { Authorization: `Bearer ${app.token}` },
      })
    ).status,
    200,
  );
});
test("agent setup remains management-only and checks the complete profile through the gateway", async () => {
  const path = `${app.origin}/api/profiles/${company.id}/agent-setup`;
  const input = {
    agent: "codex",
  };
  const headers = {
    Authorization: `Bearer ${app.token}`,
    "Content-Type": "application/json",
  };
  const denied = await fetch(path, {
    method: "POST",
    headers: {
      ...headers,
      Authorization: `Bearer ${app.store.clientKey(company.id)}`,
    },
    body: JSON.stringify(input),
  });
  assert.equal(denied.status, 401);
  const foreign = await fetch(path, {
    method: "POST",
    headers: { ...headers, Origin: "https://evil.example" },
    body: JSON.stringify(input),
  });
  assert.equal(foreign.status, 403);
  const invalid = await fetch(path, {
    method: "POST",
    headers,
    body: JSON.stringify({ ...input, model: "not-in-this-profile" }),
  });
  assert.equal(invalid.status, 400);
  const escaped = await fetch(path, {
    method: "POST",
    headers,
    body: JSON.stringify({ ...input, directory: "/tmp/unapproved" }),
  });
  assert.equal(escaped.status, 400);
  const created = await fetch(path, {
    method: "POST",
    headers,
    body: JSON.stringify(input),
  });
  assert.equal(created.status, 200);
  const setup = await created.json();
  assert.equal(
    JSON.stringify(setup).includes(app.store.clientKey(company.id)),
    false,
  );
  assert.deepEqual(setup.models, ["fixture-model"]);
  const other = await fetch(
    `${app.origin}/api/profiles/${personal.id}/agent-setup?agent=pi`,
    { headers },
  );
  assert.equal(await other.json(), null);
  const before = observed.length;
  const checked = await fetch(
    `${app.origin}/api/profiles/${company.id}/agent-check`,
    {
      method: "POST",
      headers,
      body: JSON.stringify({}),
    },
  );
  assert.equal(checked.status, 200);
  assert.deepEqual(await checked.json(), { ok: true });
  assert.equal(
    observed.length,
    before,
    "catalog check must not send an upstream inference request",
  );
  const removed = await fetch(path, {
    method: "DELETE",
    headers,
    body: JSON.stringify({ agent: "codex" }),
  });
  assert.equal(removed.status, 200);
  for (const file of setup.files)
    await assert.rejects(stat(file), { code: "ENOENT" });
});
test("dynamic agent discovery uses validated metadata and the authenticated live profile catalog", async () => {
  const original = app.models.catalog;
  const catalog = new ModelCatalog(
    Object.assign(
      async () =>
        Response.json({
          custom: {
            models: {
              "fixture-model": {
                name: "Synthetic",
                modalities: { input: ["text"], output: ["text"] },
                reasoning: false,
                tool_call: true,
                limit: { context: 64000, output: 8000 },
                cost: {
                  input: 1.25,
                  output: 5,
                  cache_read: 0.125,
                  cache_write: 1.5,
                },
              },
              "public-only": { name: "Must not appear" },
            },
          },
        }),
      fetch,
    ),
  );
  Object.defineProperty(app.models, "catalog", {
    value: catalog,
    configurable: true,
  });
  try {
    for (const agent of ["pi", "opencode"]) {
      const endpoint = `${app.endpoint(company.id)}/models?nonstopvibin=${agent}`;
      const response = await fetch(endpoint, {
        headers: {
          Authorization: `Bearer ${app.store.clientKey(company.id)}`,
        },
      });
      assert.equal(response.status, 200);
      const body = await response.json();
      assert.deepEqual(
        body.models.map((m: { id: string }) => m.id),
        ["fixture-model"],
      );
      assert.equal(body.models[0].cost.input, 1.25);
      assert.equal(
        agent === "pi"
          ? body.models[0].contextWindow
          : body.models[0].limit.context,
        64000,
      );
      assert.equal(
        (
          await fetch(endpoint, {
            headers: {
              Authorization: `Bearer ${app.store.clientKey(personal.id)}`,
            },
          })
        ).status,
        403,
      );
      const setup = await management(
        app,
        `/profiles/${company.id}/agent-setup`,
        { method: "POST", body: { agent } },
      );
      assert.equal(setup.status, 200);
      assert.deepEqual((await setup.json()).models, ["fixture-model"]);
      const remove = await management(
        app,
        `/profiles/${company.id}/agent-setup`,
        { method: "DELETE", body: { agent } },
      );
      assert.equal(remove.status, 200);
    }
  } finally {
    Object.defineProperty(app.models, "catalog", {
      value: original,
      configurable: true,
    });
  }
});

test("missing, wrong and mismatched profile credentials never reach upstream", async () => {
  const before = observed.length;
  assert.equal((await fetch(`${app.endpoint(company.id)}/models`)).status, 401);
  assert.equal(
    (
      await fetch(`${app.endpoint(company.id)}/models`, {
        headers: { Authorization: "Bearer wrong" },
      })
    ).status,
    401,
  );
  assert.equal(
    (
      await fetch(`${app.endpoint(company.id)}/models`, {
        headers: {
          Authorization: `Bearer ${app.store.clientKey(personal.id)}`,
        },
      })
    ).status,
    403,
  );
  assert.equal(
    (
      await fetch(`${app.origin}/p/${company.slug}/v0/management/config`, {
        headers: {
          Authorization: `Bearer ${app.store.clientKey(company.id)}`,
        },
      })
    ).status,
    404,
  );
  assert.equal(observed.length, before);
});
test(
  "round robin with session affinity off uses both company credentials and never personal credentials",
  { timeout: 30_000 },
  async () => {
    await withRouting(
      company,
      { strategy: "round-robin", sessionAffinity: false },
      async () => {
        const before = observed.length;
        for (let i = 0; i < 6; i++) {
          const response = await request(company);
          assert.equal(response.status, 200, await response.clone().text());
          assert.equal(
            response.headers.get("x-nonstopvibin-profile"),
            "company-a",
          );
          await response.text();
        }
        // Alternating from either credential, six requests split evenly.
        const selected = observed.slice(before);
        assert.equal(selected.filter((k) => k === "company-one").length, 3);
        assert.equal(selected.filter((k) => k === "company-two").length, 3);
      },
    );
  },
);
test("the shared /v1 endpoint routes solely by profile API key", async () => {
  const response = await fetch(`${app.origin}/v1/chat/completions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${app.store.clientKey(personal.id)}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      model: "fixture-model",
      messages: [{ role: "user", content: "hello" }],
    }),
  });
  assert.equal(response.status, 200);
  assert.match(await response.text(), /personal-one/);
});
test("SSE survives the real core and gateway with usage events and completion marker", async () => {
  const response = await request(company, {
    model: "fixture-model",
    messages: [{ role: "user", content: "hello" }],
    stream: true,
    stream_options: { include_usage: true },
  });
  assert.equal(response.status, 200);
  assert.match(
    response.headers.get("content-type") ?? "",
    /text\/event-stream/,
  );
  const text = await response.text();
  assert.match(text, /hello from company/);
  assert.match(text, /\[DONE\]/);
});
test("actual upstream accounting reaches durable SQLite exactly once", async () => {
  // Six plain requests and one stream, each answered with 14 tokens of which 3 are output.
  for (let i = 0; i < 6; i++) {
    const response = await request(accounting);
    assert.equal(response.status, 200);
    await response.text();
  }
  const stream = await request(accounting, {
    model: "fixture-model",
    messages: [{ role: "user", content: "hello" }],
    stream: true,
    stream_options: { include_usage: true },
  });
  assert.equal(stream.status, 200);
  await stream.text();
  for (let attempt = 0; attempt < 30; attempt++) {
    await app.cores.usage.collect(accounting.id);
    if (usageSummary(app.store, accounting.id).requests >= 7) break;
    await delay(50);
  }
  const summary = usageSummary(app.store, accounting.id);
  assert.equal(summary.requests, 7);
  assert.equal(summary.totalTokens, 98);
  assert.equal(summary.outputTokens, 21);
  for (const entry of app.store.usage(accounting.id, "", 300)) {
    assert.equal(entry.upstreamModel, "fixture-model");
    assert.deepEqual(entry.pricingTokens, {
      input: 6,
      cached: 5,
      cacheWrite: 0,
      output: 3,
      reasoning: 0,
    });
  }
  const raw = JSON.stringify(app.store.usage(accounting.id, "", 300));
  assert.equal(raw.includes("accounting-one"), false);
  assert.equal(raw.includes("hello"), false);
  await app.cores.usage.collect(accounting.id);
  assert.deepEqual(usageSummary(app.store, accounting.id), summary);
});
test("fill first stays on one eligible credential", async () => {
  await withRouting(
    company,
    { strategy: "fill-first", sessionAffinity: true },
    async () => {
      const before = observed.length;
      for (let i = 0; i < 4; i++) {
        const r = await request(company);
        assert.equal(r.status, 200);
        await r.text();
      }
      assert.equal(new Set(observed.slice(before)).size, 1);
    },
  );
});
test("Anthropic Messages and OpenAI Responses are translated through the profile core", async () => {
  for (const [path, payload] of [
    [
      "messages",
      {
        model: "fixture-model",
        max_tokens: 20,
        messages: [{ role: "user", content: "hello" }],
      },
    ],
    ["responses", { model: "fixture-model", input: "hello", stream: false }],
  ] as const) {
    const response = await fetch(`${app.endpoint(personal.id)}/${path}`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${app.store.clientKey(personal.id)}`,
        "Content-Type": "application/json",
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(20_000),
    });
    assert.equal(response.status, 200, await response.clone().text());
    assert.match(await response.text(), /hello from personal-one/);
  }
});
test("OAuth sessions are profile bound, cancellable, and protected from concurrent starts", async () => {
  const results = await Promise.allSettled([
    app.cores.signIn.begin(company.id, "codex"),
    app.cores.signIn.begin(personal.id, "codex"),
  ]);
  assert.equal(results.filter((r) => r.status === "fulfilled").length, 1);
  const session = results.find((r) => r.status === "fulfilled");
  assert.ok(session?.status === "fulfilled");
  const signInUrl = new URL(session.value.url);
  assert.equal(signInUrl.hostname, "auth.openai.com");
  const redirect = signInUrl.searchParams.get("redirect_uri");
  assert.ok(redirect);
  const callback = new URL(redirect);
  const malformedResponse = await new Promise<string>((resolve, reject) => {
    const socket = connect(Number(callback.port), "127.0.0.1");
    socket.once("error", reject);
    socket.once("connect", () =>
      socket.write("GET //[ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
    );
    socket.once("data", (chunk) => {
      socket.destroy();
      resolve(chunk.toString());
    });
  });
  assert.match(malformedResponse, /^HTTP\/1\.1 400/);
  await app.cores.signIn.status(session.value.profileId, session.value.state);
  const wrongProfile =
    session.value.profileId === company.id ? personal.id : company.id;
  await assert.rejects(
    app.cores.signIn.status(wrongProfile, session.value.state),
    /not found for this profile/,
  );
  await assert.rejects(
    app.cores.signIn.cancel(wrongProfile, session.value.state),
    /not found for this profile/,
  );
  await app.cores.signIn.cancel(session.value.profileId, session.value.state);
  assert.equal(app.cores.signIn.session, undefined);
  const next = await app.cores.signIn.begin(personal.id, "codex");
  await app.cores.signIn.cancel(personal.id, next.state);
});
test(
  "quota exhaustion fails inside Company A while Personal remains available",
  { timeout: 15_000 },
  async () => {
    failCompany = true;
    try {
      const before = observed.length;
      const response = await request(company);
      assert.equal(response.status, 429);
      await response.text();
      assert.ok(observed.slice(before).every((k) => k.startsWith("company")));
      const other = await request(personal);
      assert.equal(other.status, 200);
      assert.match(await other.text(), /personal-one/);
    } finally {
      failCompany = false;
      // The core keeps the 429's cooldown; a new core starts without it.
      await app.cores.pool.stop(company.id);
      await app.cores.pool.start(company.id);
    }
  },
);
test(
  "stopping a profile fails closed and a restart preserves its key and usage",
  { timeout: 15_000 },
  async () => {
    const key = app.store.clientKey(personal.id);
    await app.cores.usage.collect(personal.id);
    const summary = usageSummary(app.store, personal.id);
    await app.cores.pool.stop(personal.id);
    assert.equal((await request(personal)).status, 503);
    await app.cores.pool.start(personal.id);
    assert.equal(app.store.clientKey(personal.id), key);
    assert.deepEqual(usageSummary(app.store, personal.id), summary);
    const result = await request(personal);
    assert.equal(result.status, 200);
    await result.text();
  },
);

test("app startup refreshes an installed extension without starting its profile", async () => {
  const options = {
    ...app.options,
    directory: join(directory, "agent-update-fixture"),
    agentHome: join(directory, "agent-update-home"),
    binary: join(directory, "missing-core"),
    allowMissingCore: true,
    port: 0,
  };
  let instance = await Application.create({
    ...options,
    binary: app.options.binary,
  });
  try {
    const profile = instance.store.createProfile("Before update");
    // Agents connect to a running profile; the saved copy below stays disabled.
    await instance.cores.pool.start(profile.id);
    const installed = await instance.agentSetup.install(
      profile,
      { agent: "pi" },
      ["fixture-model"],
    );
    instance.store.saveProfile({ ...profile, name: "After update" });
    await instance.close();
    instance = await Application.create(options);
    const manifest = JSON.parse(await readFile(installed.files[1], "utf8"));
    assert.equal(manifest.port, instance.port);
    assert.equal(manifest.version, agentDefinitions.pi.version);
    const content = await readFile(installed.files[0], "utf8");
    assert.match(content, /After update/);
    assert.ok(content.includes(instance.endpoint(profile.id)));
    assert.equal(instance.state().profiles[0]?.runtime, "stopped");
    assert.deepEqual(instance.state().errors, []);
  } finally {
    await instance.close();
    await rm(instance.agentSetup.socketDirectory, {
      recursive: true,
      force: true,
    });
  }
});

test("Responses WebSocket upgrades enforce the same profile boundary", async () => {
  async function handshake(
    profile: Profile,
    key: string,
    instance = app,
  ): Promise<http.IncomingMessage> {
    return new Promise((resolve, reject) => {
      const req = http.get(`${instance.endpoint(profile.id)}/responses`, {
        headers: {
          Authorization: `Bearer ${key}`,
          Connection: "Upgrade",
          Upgrade: "websocket",
          "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
          "Sec-WebSocket-Version": "13",
        },
      });
      req.once("upgrade", (response, socket) => {
        socket.destroy();
        resolve(response);
      });
      req.once("response", (response) => {
        response.resume();
        resolve(response);
      });
      req.once("error", (error) =>
        reject(
          new Error(
            `Handshake for ${profile.slug} with ${key === instance.store.clientKey(profile.id) ? "matching" : "foreign"} key: ${error.message}`,
            { cause: error },
          ),
        ),
      );
      req.setTimeout(5000, () =>
        req.destroy(new Error("WebSocket handshake timed out")),
      );
    });
  }
  assert.equal(
    (await handshake(personal, app.store.clientKey(company.id))).statusCode,
    403,
  );
  const upgraded = await handshake(personal, app.store.clientKey(personal.id));
  assert.equal(upgraded.statusCode, 101);
  assert.equal(upgraded.headers["x-nonstopvibin-profile"], personal.slug);
  assert.ok(upgraded.headers["sec-websocket-accept"]);
  // A core that sets browser-facing headers on the handshake must not reach the agent with them.
  const misbehaving = await fakeCoreApplication(
    "misbehaving-core",
    misbehavingCore,
  );
  try {
    const { instance, profile } = misbehaving;
    const filtered = await handshake(
      profile,
      instance.store.clientKey(profile.id),
      instance,
    );
    assert.equal(filtered.statusCode, 101);
    assert.equal(filtered.headers["set-cookie"], undefined);
    assert.equal(filtered.headers["access-control-allow-origin"], undefined);
    assert.equal(filtered.headers["x-nonstopvibin-profile"], profile.slug);
    assert.equal(
      filtered.headers["sec-websocket-accept"],
      "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
    );
  } finally {
    await misbehaving.close();
  }
});

test("account pause, resume and removal apply to the running model catalog", async () => {
  await app.cores.credentials.setAccount(removal.id, "removal-one", {
    disabled: true,
  });
  assert.equal(
    (await app.cores.pool.models(removal.id)).some(
      (m) => m.id === "fixture-model",
    ),
    false,
  );
  await app.cores.credentials.setAccount(removal.id, "removal-one", {
    disabled: false,
  });
  assert.equal(
    (await app.cores.pool.models(removal.id)).some(
      (m) => m.id === "fixture-model",
    ),
    true,
  );
  await app.cores.credentials.removeAccount(removal.id, "removal-one");
  assert.equal(app.cores.accounts.list(removal.id).length, 0);
  assert.equal(
    (await app.cores.pool.models(removal.id)).some(
      (m) => m.id === "fixture-model",
    ),
    false,
  );
  assert.throws(() => app.store.apiKey("removal-one"), /Credential not found/);
});

test("the core leaves no request-body capture or error log files after traffic", async () => {
  for (const profile of [company, personal]) {
    const files = await readdir(
      profileDirectory(app.store.directory, profile.id),
      {
        recursive: true,
      },
    );
    assert.deepEqual(
      files.filter((file) =>
        /\.log$|request-body|api-request|api-response/.test(file),
      ),
      [],
    );
  }
});

test(
  "stop completes when the core exits during the final usage drain",
  { timeout: 2000 },
  async () => {
    const runtime = app.cores.pool.runtimes.get(finalDrain.id);
    assert.ok(runtime);
    intercept = async (request) => {
      const page = await httpManagement(request);
      if (request.path.startsWith("/usage-queue")) {
        const exited = once(runtime.child, "exit");
        runtime.child.kill("SIGTERM");
        await exited;
      }
      return page;
    };
    try {
      await app.cores.pool.stop(finalDrain.id);
      assert.equal(runtime.state, "stopped");
    } finally {
      intercept = undefined;
    }
  },
);

test(
  "the same API subscription can be added to multiple profiles, with duplicates scoped locally",
  { timeout: 15_000 },
  async () => {
    const profiles = [
      app.store.createProfile("Shared A"),
      app.store.createProfile("Shared B"),
    ];
    try {
      for (const profile of profiles) {
        const add = () =>
          management(app, `/profiles/${profile.id}/api-account`, {
            method: "POST",
            body: {
              name: "Shared subscription",
              provider: "custom",
              baseUrl: upstreamBase,
              apiKey: "synthetic-shared-key",
              prefix: "",
              models: [{ id: "fixture-model", protocol: "openai" }],
            },
          });
        const response = await add();
        assert.equal(response.status, 201, await response.text());
        const duplicate = await add();
        assert.equal(duplicate.status, 409);
        await duplicate.text();
        assert.equal(app.store.apiAccounts(profile.id).length, 1);
        const routed = await request(profile);
        assert.equal(routed.status, 200);
        assert.match(await routed.text(), /synthetic-shared-key/);
      }
      assert.notEqual(
        app.store.apiAccounts(profiles[0]!.id)[0]!.id,
        app.store.apiAccounts(profiles[1]!.id)[0]!.id,
      );
    } finally {
      for (const profile of profiles) {
        await app.cores.pool.stop(profile.id);
      }
    }
  },
);

test(
  "orderly concurrent stops drain every queued page and reject new gateway requests",
  { timeout: 10000 },
  async () => {
    const profile = app.store.createProfile("Drain fixture");
    await app.cores.pool.start(profile.id);
    const runtime = app.cores.pool.runtimes.get(profile.id)!;
    // Two full pages and a short one.
    const queue = Array.from({ length: 2 * usagePageSize + 1 }, (_, i) => ({
      request_id: `drain-${i}`,
      timestamp: new Date().toISOString(),
      provider: "codex",
      model: "fixture",
      tokens: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
    }));
    let pages = 0;
    const fixture = http.createServer(async (req, res) => {
      const url = new URL(req.url ?? "/", "http://127.0.0.1");
      if (url.pathname !== "/v0/management/usage-queue") {
        res.writeHead(404).end();
        return;
      }
      pages++;
      assert.throws(() => app.cores.pool.port(profile.id), /stopped/);
      // A gateway request finishing during the stop makes a scheduled drain due at once.
      app.cores.usage.markActivity(profile.id);
      // Yield so both stop callers overlap the same destructive drain.
      await delay(10);
      res.setHeader("Content-Type", "application/json");
      res.end(
        JSON.stringify(queue.splice(0, Number(url.searchParams.get("count")))),
      );
    });
    const fixturePort = await listen(fixture);
    // Finish any poll of the real core before replacing its queue with the fixture.
    await app.cores.usage.collect(profile.id);
    intercept = (request) =>
      httpManagement(
        request.profileId === profile.id
          ? { ...request, port: fixturePort }
          : request,
      );
    const coalesce = appUsageTiming.coalesce;
    appUsageTiming.coalesce = 0;
    // As on a slow runner, the core takes a while to exit after the stop's final drain.
    const kill = runtime.child.kill.bind(runtime.child);
    runtime.child.kill = (signal) => {
      setTimeout(() => kill(signal), 300);
      return true;
    };
    try {
      await Promise.all([
        app.cores.pool.stop(profile.id),
        app.cores.pool.stop(profile.id),
      ]);
      // Only the stop reads a stopping core's queue: two full pages, then the short page.
      assert.equal(pages, 3);
      assert.equal(queue.length, 0);
      assert.equal(
        usageSummary(app.store, profile.id).requests,
        2 * usagePageSize + 1,
      );
      assert.equal(runtime.state, "stopped");
      assert.ok(
        runtime.child.exitCode !== null || runtime.child.signalCode !== null,
      );
    } finally {
      appUsageTiming.coalesce = coalesce;
      intercept = undefined;
      fixture.close();
      fixture.closeAllConnections();
      await app.cores.pool.stop(profile.id);
    }
  },
);

test("activity API preserves one snapshot while usage arrives during aggregation", async () => {
  const profile = app.store.createProfile("Activity snapshot");
  const usage = {
    profileId: profile.id,
    timestamp: new Date().toISOString(),
    provider: "claude",
    model: "fixture",
    account: "fixture",
    inputTokens: 100,
    cachedTokens: 0,
    cacheWriteTokens: 0,
    outputTokens: 0,
    reasoningTokens: 0,
    totalTokens: 100,
    latencyMs: 1,
    failed: false,
    statusCode: 200,
    stream: false,
  };
  app.store.addUsage(
    Array.from({ length: 1201 }, (_, i) => ({
      ...usage,
      id: `api-snapshot-${i}`,
    })),
  );
  const read = app.store.usageBetween.bind(app.store);
  let signal!: () => void;
  const reading = new Promise<void>((resolve) => {
    signal = resolve;
  });
  app.store.usageBetween = (...args) => {
    const batch = read(...args);
    if (args[0] === profile.id) signal();
    return batch;
  };
  try {
    const pending = management(
      app,
      `/profiles/${profile.id}/usage?days=${allHistoryDays}`,
    );
    await reading;
    app.store.addUsage([
      {
        ...usage,
        id: "api-concurrent",
        inputTokens: 1000000,
        totalTokens: 1000000,
      },
    ]);
    const response = await pending;
    assert.equal(response.status, 200);
    const result = await response.json();
    assert.equal(result.summary.requests, 1201);
    assert.equal(result.pricing.totals.requests, result.summary.requests);
    assert.equal(
      result.pricing.totals.tokens.input,
      result.summary.totalTokens,
    );
    assert.equal(
      result.records.some((r: { id: string }) => r.id === "api-concurrent"),
      false,
    );
  } finally {
    app.store.usageBetween = read;
  }
});

test(
  "agent aborts and an unreachable core are not reported as application faults",
  { timeout: 15_000 },
  async () => {
    const profile = app.store.createProfile("Abort fixture");
    app.store.saveApiAccount(
      profile.id,
      customAccount("abort-one", upstreamBase, [
        { id: "fixture-model", protocol: "openai" },
      ]),
      "abort-one",
    );
    const proxy = app.gateway.proxy;
    let proxied: Promise<void> | undefined;
    let agentSocket: Socket | undefined;
    app.gateway.proxy = (req, res) => {
      agentSocket = req.socket;
      return (proxied = proxy.call(app.gateway, req, res));
    };
    const settled = async () => {
      await proxied?.catch(() => {});
      // Let the server's request catch observe the settled proxy.
      await new Promise((resolve) => setImmediate(resolve));
    };
    const reported = [...app.errors];
    try {
      await app.cores.pool.start(profile.id);
      const hung = new Promise<void>((resolve) => {
        onHang = resolve;
      });
      const pending = request(profile, {
        model: "fixture-model",
        messages: [{ role: "user", content: "hang" }],
      }).then(
        () => assert.fail("an abandoned request has no response"),
        () => undefined,
      );
      await hung;
      // The agent's connection drops before any response header exists. Bun only
      // notices a peer close lazily, so drop it from the server side, as Node would.
      assert.ok(agentSocket);
      agentSocket.destroy();
      await pending;
      await settled();
      assert.equal(app.gateway.active.get(profile.id), 0);
      assert.deepEqual(app.errors, reported);

      const unreachable = await fakeCoreApplication(
        "unreachable-core",
        idleCore,
        fakeManagement,
      );
      try {
        const { instance, profile } = unreachable;
        const response = await fetch(
          `${instance.endpoint(profile.id)}/chat/completions`,
          {
            method: "POST",
            headers: {
              Authorization: `Bearer ${instance.store.clientKey(profile.id)}`,
              "Content-Type": "application/json",
            },
            body: JSON.stringify({ model: "fixture-model", messages: [] }),
          },
        );
        assert.equal(response.status, 502);
        const body = await response.text();
        assert.match(
          body,
          /profile proxy is unavailable\. Restart the profile from the sidebar if NonstopVibin offers it; otherwise quit and reopen NonstopVibin\./,
        );
        assert.doesNotMatch(body, /ECONNREFUSED|hang up|Premature/i);
        assert.deepEqual(instance.errors, []);
      } finally {
        await unreachable.close();
      }
    } finally {
      onHang = undefined;
      app.gateway.proxy = proxy;
      await app.cores.pool.stop(profile.id);
    }
  },
);
