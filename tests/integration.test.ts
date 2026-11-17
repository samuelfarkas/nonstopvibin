// node:test hooks ignore their timeout option under Bun (5 s default); these start and stop cores.
import { afterAll, beforeAll } from "bun:test";
import { test } from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SettingsPage } from "../src/client/Settings.tsx";
import coreRelease from "../scripts/core-release.json";
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
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { ModelCatalog } from "../src/server/model-catalog.ts";
import { Application } from "../src/server/server.ts";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import type { Json, Profile } from "../src/shared/types.ts";
import { record } from "../src/server/json.ts";
import { settle } from "./teardown.ts";

let app: Application;
let company: Profile;
let personal: Profile;
let directory: string;
let failCompany = false;
let onHang: (() => void) | undefined;
const observed: string[] = [];
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
      Authorization: `Bearer ${app.store.secret(`${profile.id}:client`)}`,
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
  await new Promise<void>((resolve) =>
    upstream.listen(0, "127.0.0.1", resolve),
  );
  const addr = upstream.address();
  assert.ok(addr instanceof Object);
  upstreamBase = `http://127.0.0.1:${addr.port}/v1`;
  app = await Application.create({
    agentHome: join(directory, "agent-home"),
    directory,
    binary: resolve(".vendor/core/cli-proxy-api"),
    clientDirectory: resolve("dist/client"),
    port: 0,
  });
  company = app.store.createProfile("Company A", "forest");
  personal = app.store.createProfile("Personal", "blue");
  for (const [profile, keys] of [
    [company, ["company-one", "company-two"]],
    [personal, ["personal-one"]],
  ] as const)
    for (const key of keys)
      app.store.saveApiAccount(
        profile.id,
        {
          id: key,
          name: key,
          provider: "custom",
          baseUrl: upstreamBase,
          prefix: "",
          disabled: false,
          models: [{ id: "fixture-model", protocol: "openai" }],
        },
        key,
      );
  await app.core.start(company.id);
  await app.core.start(personal.id);
}, 30_000);
// A core may need its usage drain plus the 5 s SIGTERM grace (timing.kill) to stop;
// settle names whatever is still running well before this hook's own limit.
afterAll(async () => {
  if (app) await settle("app.close()", app.close(), app.core, 25_000);
  if (upstream)
    await new Promise<void>((resolve) => {
      upstream.close(() => resolve());
      upstream.closeAllConnections();
    });
  if (directory) await rm(directory, { recursive: true, force: true });
  if (app)
    await rm(app.agentSetup.socketDirectory, {
      recursive: true,
      force: true,
    });
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
  const response = await fetch(
    `${app.origin}/api/profiles/${company.id}/accounts/%E0`,
    {
      method: "DELETE",
      headers: { Authorization: `Bearer ${app.token}` },
    },
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
  const profile = app.store.createProfile("Key rotation fixture", "plum");
  const bystander = app.store.createProfile("Key bystander", "plum");
  const status = async (key: string) => {
    const response = await fetch(`${app.origin}/v1/models`, {
      headers: { Authorization: `Bearer ${key}` },
    });
    await response.text();
    return response.status;
  };
  // Accepted keys reach the stopped-profile check; rejected keys never do.
  // Stopped profiles keep this off the running cores, whose activity later tests time.
  const original = app.store.secret(`${profile.id}:client`);
  assert.equal(await status(original), 503);
  const rotated = `nv_${"R".repeat(43)}`;
  app.store.setSecret(`${profile.id}:client`, rotated);
  assert.equal(await status(original), 401);
  assert.equal(await status(rotated), 503);
  // Removal outside the Store's write methods must fail closed too.
  app.store.db.prepare("DELETE FROM profiles WHERE id=?").run(profile.id);
  assert.equal(await status(rotated), 401);
  app.store.db
    .prepare("DELETE FROM secrets WHERE id LIKE ?")
    .run(`${profile.id}:%`);
  assert.equal(await status(rotated), 401);
  assert.equal(await status(app.store.secret(`${bystander.id}:client`)), 503);
});
test("static assets revalidate without caching management responses", async () => {
  const original = app.options.clientDirectory;
  const client = join(directory, "static-fixture");
  await mkdir(join(client, "assets"), { recursive: true });
  await writeFile(join(client, "index.html"), "fixture page");
  await writeFile(join(client, "assets/app-AbCd1234.js"), "fixture script");
  app.options.clientDirectory = client;
  try {
    const first = await fetch(app.origin);
    assert.equal(await first.text(), "fixture page");
    assert.equal(first.headers.get("cache-control"), "no-cache");
    const etag = first.headers.get("etag")!;
    const restarted = await Application.create({
      ...app.options,
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
      const cached = await fetch(app.origin, {
        headers: { "If-None-Match": match },
      });
      assert.equal(cached.status, 304);
      assert.equal(await cached.text(), "");
      assert.equal(cached.headers.get("etag"), etag);
      assert.ok(cached.headers.has("content-security-policy"));
    }
    await writeFile(join(client, "index.html"), "updated fixture page");
    const updated = await fetch(app.origin, {
      headers: { "If-None-Match": etag },
    });
    assert.equal(updated.status, 200);
    assert.equal(await updated.text(), "updated fixture page");
    const asset = await fetch(`${app.origin}/assets/app-AbCd1234.js`);
    assert.equal(await asset.text(), "fixture script");
    assert.equal(
      asset.headers.get("cache-control"),
      "public, max-age=31536000, immutable",
    );
    const head = await fetch(app.origin, { method: "HEAD" });
    assert.equal(head.status, 200);
    assert.equal(await head.text(), "");
    const state = await fetch(`${app.origin}/api/state`, {
      headers: { Authorization: `Bearer ${app.token}`, "If-None-Match": "*" },
    });
    assert.equal(state.status, 200);
    assert.equal(state.headers.get("cache-control"), "no-store");
    const missing = await fetch(`${app.origin}/missing.js`, {
      headers: { "If-None-Match": "*" },
    });
    assert.equal(missing.status, 404);
  } finally {
    app.options.clientDirectory = original;
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
  const config = record(await app.core.management(company.id, "/config"));
  assert.equal(config["commercial-mode"], true);
  assert.equal(config["usage-statistics-enabled"], true);
  assert.notEqual(app.core.port(company.id), app.core.port(personal.id));
  assert.ok(
    (await app.core.models(company.id)).some((m) => m.id === "fixture-model"),
  );
  for (const profile of [company, personal]) {
    const config = join(app.core.directory(profile.id), "config.yaml");
    assert.equal((await stat(config)).mode & 0o777, 0o600);
    assert.equal(
      (await stat(join(app.core.directory(profile.id), "auth"))).mode & 0o777,
      0o700,
    );
  }
  const raw = await readFile(join(directory, "nonstopvibin.sqlite"));
  assert.equal(
    raw.includes(Buffer.from(app.store.secret(`${company.id}:client`))),
    false,
  );
});

test("usage retention management rejects profile keys and invalid limits", async () => {
  const put = (key: string, days: number) =>
    fetch(`${app.origin}/api/usage-retention`, {
      method: "PUT",
      headers: {
        Authorization: `Bearer ${key}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ days }),
    });
  assert.equal(
    (await put(app.store.secret(`${company.id}:client`), 30)).status,
    401,
  );
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
    const core = new CorePool(store, app.options.binary);
    core.timing.accounts = 200;
    core.timing.coalesce = 20;
    const profile = store.createProfile("Maintenance fixture", "forest");
    const syncAccounts = core.syncAccounts;
    const collectUsage = core.collectUsage;
    let enter!: () => void;
    let resume!: () => void;
    const entered = new Promise<void>((resolve) => {
      enter = resolve;
    });
    const release = new Promise<void>((resolve) => {
      resume = resolve;
    });
    let collections = 0;
    let collected = () => {};
    try {
      await core.start(profile.id);
      core.syncAccounts = async (id) => {
        enter();
        await release;
        return syncAccounts.call(core, id);
      };
      core.collectUsage = async (id) => {
        collections++;
        collected();
        return collectUsage.call(core, id);
      };
      core.beginPolling();
      await entered;
      assert.ok(
        collections <= 2,
        "idle profiles do not authenticate every two seconds",
      );
      const before = collections;
      const next = new Promise<void>((resolve) => {
        collected = resolve;
      });
      core.markUsageActivity(profile.id);
      await next;
      assert.ok(
        collections > before,
        "accounting continues while maintenance is waiting",
      );
    } finally {
      resume();
      await core.shutdown();
      store.close();
    }
  },
);
test("concurrent usage drains share one destructive queue read", async () => {
  const first = app.core.collectUsage(company.id);
  const second = app.core.collectUsage(company.id);
  assert.equal(first, second);
  await first;
});
test("verified CLIProxyAPI exposes its native Codex catalog through the same profile", async () => {
  const response = await fetch(
    `${app.endpoint(company.id)}/models?client_version=0.140.0`,
    {
      headers: {
        Authorization: `Bearer ${app.store.secret(`${company.id}:client`)}`,
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
      Authorization: `Bearer ${app.store.secret(`${company.id}:client`)}`,
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
    JSON.stringify(setup).includes(app.store.secret(`${company.id}:client`)),
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
  const original = app.gateway.catalog;
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
  Object.defineProperty(app.gateway, "catalog", {
    value: catalog,
    configurable: true,
  });
  try {
    for (const agent of ["pi", "opencode"]) {
      const endpoint = `${app.endpoint(company.id)}/models?nonstopvibin=${agent}`;
      const response = await fetch(endpoint, {
        headers: {
          Authorization: `Bearer ${app.store.secret(`${company.id}:client`)}`,
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
              Authorization: `Bearer ${app.store.secret(`${personal.id}:client`)}`,
            },
          })
        ).status,
        403,
      );
      const setup = await fetch(
        `${app.origin}/api/profiles/${company.id}/agent-setup`,
        {
          method: "POST",
          headers: {
            Authorization: `Bearer ${app.token}`,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({ agent }),
        },
      );
      assert.equal(setup.status, 200);
      assert.deepEqual((await setup.json()).models, ["fixture-model"]);
      const remove = await fetch(
        `${app.origin}/api/profiles/${company.id}/agent-setup`,
        {
          method: "DELETE",
          headers: {
            Authorization: `Bearer ${app.token}`,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({ agent }),
        },
      );
      assert.equal(remove.status, 200);
    }
  } finally {
    Object.defineProperty(app.gateway, "catalog", {
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
          Authorization: `Bearer ${app.store.secret(`${personal.id}:client`)}`,
        },
      })
    ).status,
    403,
  );
  assert.equal(
    (
      await fetch(`${app.origin}/p/${company.slug}/v0/management/config`, {
        headers: {
          Authorization: `Bearer ${app.store.secret(`${company.id}:client`)}`,
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
    const changed = await fetch(`${app.origin}/api/profiles/${company.id}`, {
      method: "PATCH",
      headers: {
        Authorization: `Bearer ${app.token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ strategy: "round-robin", sessionAffinity: false }),
    });
    assert.equal(changed.status, 200, await changed.text());
    const before = observed.length;
    for (let i = 0; i < 6; i++) {
      const response = await request(company);
      assert.equal(response.status, 200, await response.clone().text());
      assert.equal(response.headers.get("x-nonstopvibin-profile"), "company-a");
      await response.text();
    }
    const selected = observed.slice(before);
    assert.equal(selected.filter((k) => k === "company-one").length, 3);
    assert.equal(selected.filter((k) => k === "company-two").length, 3);
  },
);
test("the shared /v1 endpoint routes solely by profile API key", async () => {
  const response = await fetch(`${app.origin}/v1/chat/completions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${app.store.secret(`${personal.id}:client`)}`,
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
  for (let attempt = 0; attempt < 30; attempt++) {
    await app.core.collectUsage(company.id);
    if (app.store.summary(company.id).requests >= 7) break;
    await delay(50);
  }
  const summary = app.store.summary(company.id);
  assert.equal(summary.requests, 7);
  assert.equal(summary.totalTokens, 98);
  assert.equal(summary.outputTokens, 21);
  for (const entry of app.store.usage(company.id)) {
    assert.equal(entry.upstreamModel, "fixture-model");
    assert.deepEqual(entry.pricingTokens, {
      input: 6,
      cached: 5,
      cacheWrite: 0,
      output: 3,
      reasoning: 0,
    });
  }
  const raw = JSON.stringify(app.store.usage(company.id));
  assert.equal(raw.includes("company-one"), false);
  assert.equal(raw.includes("hello"), false);
  await app.core.collectUsage(company.id);
  assert.deepEqual(app.store.summary(company.id), summary);
});
test("fill first stays on one eligible credential", async () => {
  const changed = await fetch(`${app.origin}/api/profiles/${company.id}`, {
    method: "PATCH",
    headers: {
      Authorization: `Bearer ${app.token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ strategy: "fill-first", sessionAffinity: true }),
  });
  assert.equal(changed.status, 200, await changed.text());
  const before = observed.length;
  for (let i = 0; i < 4; i++) {
    const r = await request(company);
    assert.equal(r.status, 200);
    await r.text();
  }
  assert.equal(new Set(observed.slice(before)).size, 1);
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
        Authorization: `Bearer ${app.store.secret(`${personal.id}:client`)}`,
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
    app.core.beginOAuth(company.id, "codex"),
    app.core.beginOAuth(personal.id, "codex"),
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
  await app.core.oauthStatus(session.value.profileId, session.value.state);
  const wrongProfile =
    session.value.profileId === company.id ? personal.id : company.id;
  await assert.rejects(
    app.core.oauthStatus(wrongProfile, session.value.state),
    /not found for this profile/,
  );
  await assert.rejects(
    app.core.cancelOAuth(wrongProfile, session.value.state),
    /not found for this profile/,
  );
  await app.core.cancelOAuth(session.value.profileId, session.value.state);
  assert.equal(app.core.oauth, undefined);
  const next = await app.core.beginOAuth(personal.id, "codex");
  await app.core.cancelOAuth(personal.id, next.state);
});
test("quota exhaustion fails inside Company A while Personal remains available", async () => {
  failCompany = true;
  const before = observed.length;
  const response = await request(company);
  assert.equal(response.status, 429);
  await response.text();
  assert.ok(observed.slice(before).every((k) => k.startsWith("company")));
  const other = await request(personal);
  assert.equal(other.status, 200);
  assert.match(await other.text(), /personal-one/);
});
test(
  "stopping a profile fails closed and a restart preserves its key and usage",
  { timeout: 15_000 },
  async () => {
    const key = app.store.secret(`${personal.id}:client`);
    await app.core.collectUsage(personal.id);
    const summary = app.store.summary(personal.id);
    await app.core.stop(personal.id);
    assert.equal((await request(personal)).status, 503);
    await app.core.start(personal.id);
    assert.equal(app.store.secret(`${personal.id}:client`), key);
    assert.deepEqual(app.store.summary(personal.id), summary);
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
    port: 0,
  };
  let instance = await Application.create(options);
  try {
    const profile = instance.store.createProfile("Before update", "blue");
    const installed = await instance.agentSetup.install(
      profile,
      { agent: "pi" },
      ["fixture-model"],
      instance.port,
    );
    instance.store.saveProfile({ ...profile, name: "After update" });
    await instance.close();
    instance = await Application.create(options);
    const manifest = JSON.parse(await readFile(installed.files[1], "utf8"));
    assert.equal(manifest.port, instance.port);
    assert.equal(manifest.version, 7);
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
  ): Promise<http.IncomingMessage> {
    return new Promise((resolve, reject) => {
      const req = http.get(`${app.endpoint(profile.id)}/responses`, {
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
            `Handshake for ${profile.slug} with ${key === app.store.secret(`${profile.id}:client`) ? "matching" : "foreign"} key: ${error.message}`,
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
    (await handshake(personal, app.store.secret(`${company.id}:client`)))
      .statusCode,
    403,
  );
  const upgraded = await handshake(
    personal,
    app.store.secret(`${personal.id}:client`),
  );
  assert.equal(upgraded.statusCode, 101);
  assert.equal(upgraded.headers["x-nonstopvibin-profile"], personal.slug);
  assert.ok(upgraded.headers["sec-websocket-accept"]);
  // A core that sets browser-facing headers on the handshake must not reach the agent with them.
  const runtime = app.core.runtimes.get(personal.id)!;
  const fixture = http.createServer();
  fixture.on("upgrade", (_req, socket) => {
    socket.end(
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nAccess-Control-Allow-Origin: *\r\nX-NonstopVibin-Profile: forged\r\n\r\n",
    );
  });
  await new Promise<void>((resolve) => fixture.listen(0, "127.0.0.1", resolve));
  const address = fixture.address();
  assert.ok(address instanceof Object);
  const port = runtime.port;
  runtime.port = address.port;
  try {
    const filtered = await handshake(
      personal,
      app.store.secret(`${personal.id}:client`),
    );
    assert.equal(filtered.statusCode, 101);
    assert.equal(filtered.headers["set-cookie"], undefined);
    assert.equal(filtered.headers["access-control-allow-origin"], undefined);
    assert.equal(filtered.headers["x-nonstopvibin-profile"], personal.slug);
    assert.equal(
      filtered.headers["sec-websocket-accept"],
      "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
    );
  } finally {
    runtime.port = port;
    fixture.close();
    fixture.closeAllConnections();
  }
});

test("account pause, resume and removal apply to the running model catalog", async () => {
  await app.core.setAccount(personal.id, "personal-one", { disabled: true });
  assert.equal(
    (await app.core.models(personal.id)).some((m) => m.id === "fixture-model"),
    false,
  );
  await app.core.setAccount(personal.id, "personal-one", { disabled: false });
  assert.equal(
    (await app.core.models(personal.id)).some((m) => m.id === "fixture-model"),
    true,
  );
  await app.core.removeAccount(personal.id, "personal-one");
  assert.equal(app.core.accounts(personal.id).length, 0);
  assert.equal(
    (await app.core.models(personal.id)).some((m) => m.id === "fixture-model"),
    false,
  );
  assert.throws(
    () => app.store.secret("personal-one:api"),
    /Credential not found/,
  );
});

test("the core leaves no request-body capture or error log files after traffic", async () => {
  for (const profile of [company, personal]) {
    const files = await readdir(app.core.directory(profile.id), {
      recursive: true,
    });
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
    const runtime = app.core.runtimes.get(personal.id);
    assert.ok(runtime);
    const collectUsage = app.core.collectUsage;
    app.core.collectUsage = async (profileId) => {
      const count = await collectUsage.call(app.core, profileId);
      const exited = once(runtime.child, "exit");
      runtime.child.kill("SIGTERM");
      await exited;
      return count;
    };
    try {
      await app.core.stop(personal.id);
      assert.equal(runtime.state, "stopped");
      assert.equal(app.store.profile(personal.id).enabled, false);
    } finally {
      app.core.collectUsage = collectUsage;
    }
  },
);

test(
  "the same API subscription can be added to multiple profiles, with duplicates scoped locally",
  { timeout: 15_000 },
  async () => {
    const profiles = [
      app.store.createProfile("Shared A", "forest"),
      app.store.createProfile("Shared B", "blue"),
    ];
    try {
      for (const profile of profiles) {
        const add = () =>
          fetch(`${app.origin}/api/profiles/${profile.id}/api-account`, {
            method: "POST",
            headers: {
              Authorization: `Bearer ${app.token}`,
              "Content-Type": "application/json",
            },
            body: JSON.stringify({
              name: "Shared subscription",
              provider: "custom",
              baseUrl: upstreamBase,
              apiKey: "synthetic-shared-key",
              prefix: "",
              models: [{ id: "fixture-model", protocol: "openai" }],
            }),
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
        await app.core.stop(profile.id);
      }
    }
  },
);

test(
  "orderly concurrent stops drain every queued page and reject new gateway requests",
  { timeout: 10000 },
  async () => {
    const profile = app.store.createProfile("Drain fixture", "forest");
    await app.core.start(profile.id);
    const runtime = app.core.runtimes.get(profile.id)!;
    const queue = Array.from({ length: 1001 }, (_, i) => ({
      request_id: `drain-${i}`,
      timestamp: new Date().toISOString(),
      provider: "codex",
      model: "fixture",
      tokens: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
    }));
    let pages = 0;
    const fixture = http.createServer(async (req, res) => {
      if (!req.url?.startsWith("/v0/management/usage-queue")) {
        res.writeHead(404).end();
        return;
      }
      pages++;
      assert.throws(() => app.core.port(profile.id), /stopped/);
      // A gateway request finishing during the stop makes a scheduled drain due at once.
      app.core.markUsageActivity(profile.id);
      // Yield so both stop callers overlap the same destructive drain.
      await delay(10);
      res.setHeader("Content-Type", "application/json");
      res.end(JSON.stringify(queue.splice(0, 500)));
    });
    await new Promise<void>((resolve) =>
      fixture.listen(0, "127.0.0.1", resolve),
    );
    const address = fixture.address();
    assert.ok(address instanceof Object);
    // Finish any poll of the real core before replacing its queue with the fixture.
    await app.core.collectUsage(profile.id);
    runtime.port = address.port;
    const coalesce = app.core.timing.coalesce;
    app.core.timing.coalesce = 0;
    // As on a slow runner, the core takes a while to exit after the stop's final drain.
    const kill = runtime.child.kill.bind(runtime.child);
    runtime.child.kill = (signal) => {
      setTimeout(() => kill(signal), 300);
      return true;
    };
    try {
      await Promise.all([app.core.stop(profile.id), app.core.stop(profile.id)]);
      // Only the stop reads a stopping core's queue: 500, 500, then the short page.
      assert.equal(pages, 3);
      assert.equal(queue.length, 0);
      assert.equal(app.store.summary(profile.id).requests, 1001);
      assert.equal(runtime.state, "stopped");
      assert.ok(
        runtime.child.exitCode !== null || runtime.child.signalCode !== null,
      );
    } finally {
      app.core.timing.coalesce = coalesce;
      fixture.close();
      fixture.closeAllConnections();
      await app.core.stop(profile.id);
    }
  },
);

test("activity API preserves one snapshot while usage arrives during aggregation", async () => {
  const profile = app.store.createProfile("Activity snapshot", "forest");
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
  const read = app.store.usageTimeBatch.bind(app.store);
  let signal!: () => void;
  const reading = new Promise<void>((resolve) => {
    signal = resolve;
  });
  app.store.usageTimeBatch = (...args) => {
    const batch = read(...args);
    if (args[0] === profile.id) signal();
    return batch;
  };
  try {
    const pending = fetch(
      `${app.origin}/api/profiles/${profile.id}/usage?days=3650`,
      { headers: { Authorization: `Bearer ${app.token}` } },
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
    app.store.usageTimeBatch = read;
  }
});

test(
  "agent aborts and an unreachable core are not reported as application faults",
  { timeout: 15_000 },
  async () => {
    const profile = app.store.createProfile("Abort fixture", "clay");
    app.store.saveApiAccount(
      profile.id,
      {
        id: "abort-one",
        name: "abort-one",
        provider: "custom",
        baseUrl: upstreamBase,
        prefix: "",
        disabled: false,
        models: [{ id: "fixture-model", protocol: "openai" }],
      },
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
    const reported = [...app.core.errors];
    try {
      await app.core.start(profile.id);
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
      assert.deepEqual(app.core.errors, reported);

      const runtime = app.core.runtimes.get(profile.id)!;
      const closed = http.createServer();
      await new Promise<void>((resolve) =>
        closed.listen(0, "127.0.0.1", resolve),
      );
      const address = closed.address();
      assert.ok(address instanceof Object);
      await new Promise<void>((resolve) => closed.close(() => resolve()));
      const port = runtime.port;
      runtime.port = address.port;
      try {
        const response = await request(profile);
        await settled();
        assert.equal(response.status, 502);
        const body = await response.text();
        assert.match(
          body,
          /profile proxy is unavailable\. Restart the profile from the sidebar if NonstopVibin offers it; otherwise quit and reopen NonstopVibin\./,
        );
        assert.doesNotMatch(body, /ECONNREFUSED|hang up|Premature/i);
        assert.deepEqual(app.core.errors, reported);
      } finally {
        runtime.port = port;
      }
    } finally {
      onHang = undefined;
      app.gateway.proxy = proxy;
      await app.core.stop(profile.id);
    }
  },
);
