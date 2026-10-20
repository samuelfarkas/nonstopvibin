import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { connect } from "node:net";
import { mkdtemp, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { Application } from "../src/server/server.ts";
import { record } from "../src/server/json.ts";
import type { JsonObject, Profile } from "../src/shared/types.ts";

// Mock only the external Anthropic provider. Every request below traverses the
// real application gateway and the verified, compiled release-pinned core.
const model = "claude-sonnet-4-6";
const inputs = [{ path: "one.txt" }, { path: 'two "quoted" π.txt' }];
const image = {
  type: "image",
  source: {
    type: "base64",
    media_type: "image/png",
    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aT1sAAAAASUVORK5CYII=",
  },
};
const frame = (type: string, fields: JsonObject = {}) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...fields })}\n\n`;
const start = frame("message_start", {
  message: {
    id: "msg_native_fixture",
    type: "message",
    role: "assistant",
    model,
    content: [],
    stop_reason: null,
    stop_sequence: null,
    usage: { input_tokens: 12, output_tokens: 0 },
  },
});
function toolStream() {
  let wire = start;
  for (const [index, input] of inputs.entries()) {
    wire += frame("content_block_start", {
      index,
      content_block: {
        type: "tool_use",
        id: `toolu_native_${index}`,
        name: "read_fixture",
        input: {},
      },
    });
    const json = JSON.stringify(input);
    for (const partial_json of [json.slice(0, 9), json.slice(9)])
      wire += frame("content_block_delta", {
        index,
        delta: { type: "input_json_delta", partial_json },
      });
    wire += frame("content_block_stop", { index });
  }
  return (
    wire +
    frame("message_delta", {
      delta: { stop_reason: "tool_use", stop_sequence: null },
      usage: { output_tokens: 8 },
    }) +
    frame("message_stop")
  );
}
function events(wire: string): JsonObject[] {
  return wire
    .split("\n")
    .filter((line) => line.startsWith("data: {"))
    .map((line) => JSON.parse(line.slice(6)));
}
let app: Application;
let directory: string;
let upstream: http.Server;
let primary: Profile;
let other: Profile;
let exhausted: Profile;
let providerUrl: string;
let heldConnections = 0;
const observed: {
  path: string;
  body: ReturnType<typeof JSON.parse>;
  headers: http.IncomingHttpHeaders;
}[] = [];
function addAccount(profile: Profile) {
  app.store.saveApiAccount(
    profile.id,
    {
      id: profile.id,
      name: "Synthetic native Anthropic",
      provider: "custom",
      baseUrl: providerUrl,
      prefix: "",
      disabled: false,
      models: [{ id: model, protocol: "anthropic" }],
    },
    `synthetic-native-${profile.slug}`,
  );
}
function headers(profile = primary) {
  return {
    Authorization: `Bearer ${app.store.secret(`${profile.id}:client`)}`,
    "Content-Type": "application/json",
    "anthropic-version": "2023-06-01",
  };
}
function send(
  body: JsonObject,
  profile = primary,
  path = "messages",
  extraHeaders: HeadersInit = headers(profile),
) {
  return fetch(`${app.endpoint(profile.id)}/${path}`, {
    method: "POST",
    headers: extraHeaders,
    body: JSON.stringify({ model, max_tokens: 64, ...body }),
    signal: AbortSignal.timeout(10000),
  });
}
const message = (text: string) => [{ role: "user", content: text }];
before(
  async () => {
    directory = await mkdtemp("/tmp/nv-native-anthropic-");
    upstream = http.createServer(async (req, res) => {
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(Buffer.from(chunk));
      const body = JSON.parse(Buffer.concat(chunks).toString());
      observed.push({ path: req.url ?? "", body, headers: req.headers });
      if (
        req.headers.authorization ===
        `Bearer synthetic-native-${exhausted.slug}`
      ) {
        res.writeHead(429, {
          "Content-Type": "application/json",
          "Retry-After": "60",
        });
        res.end(
          JSON.stringify({
            type: "error",
            error: {
              type: "rate_limit_error",
              message: "synthetic quota exhausted",
            },
          }),
        );
        return;
      }
      const content = body.messages[0].content;
      const label = Array.isArray(content)
        ? content.find((block: JsonObject) => block.type === "text")?.text
        : content;
      if (label === "reject") {
        res.writeHead(400, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            type: "error",
            error: {
              type: "invalid_request_error",
              message: "synthetic invalid request",
            },
            request_id: "req_fixture",
          }),
        );
      } else if (body.stream) {
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Access-Control-Allow-Origin": "https://fixture.invalid",
          "Set-Cookie": "fixture=not-forwarded",
        });
        if (label === "hold") {
          heldConnections++;
          res.once("close", () => heldConnections--);
          res.write(start);
          return; // Deliberately held until the real client's disconnect cancels it.
        }
        if (label === "truncated") res.end(start);
        else if (label === "stream-error")
          res.end(
            start +
              frame("error", {
                error: {
                  type: "overloaded_error",
                  message: "synthetic stream failure",
                },
              }),
          );
        else {
          const wire = toolStream();
          // Fragment transport chunks inside JSON and SSE delimiters.
          res.write(wire.slice(0, 37));
          res.write(wire.slice(37, 421));
          res.end(wire.slice(421));
        }
      } else {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            id: "msg_native_fixture",
            type: "message",
            role: "assistant",
            model,
            content: [{ type: "text", text: "fixture replay accepted" }],
            stop_reason: "end_turn",
            stop_sequence: null,
            usage: { input_tokens: 12, output_tokens: 8 },
          }),
        );
      }
    });
    await new Promise<void>((resolve) =>
      upstream.listen(0, "127.0.0.1", resolve),
    );
    const address = upstream.address();
    assert.ok(address instanceof Object);
    providerUrl = `http://127.0.0.1:${address.port}`;
    app = await Application.create({
      directory,
      agentHome: join(directory, "home"),
      binary: resolve(".vendor/core/cli-proxy-api"),
      clientDirectory: resolve("dist/client"),
      port: 0,
      requirePinnedCore: true,
    });
    primary = app.store.createProfile("Native fixture", "forest");
    other = app.store.createProfile("Other native fixture", "blue");
    exhausted = app.store.createProfile("Exhausted native fixture", "forest");
    for (const profile of [primary, other, exhausted]) {
      addAccount(profile);
      await app.core.start(profile.id);
    }
  },
  { timeout: 30000 },
);
after(async () => {
  if (app) {
    await app.close();
    await rm(app.agentSetup.socketDirectory, { recursive: true, force: true });
  }
  if (upstream)
    await new Promise<void>((resolve) => {
      upstream.close(() => resolve());
      upstream.closeAllConnections();
    });
  if (directory) await rm(directory, { recursive: true, force: true });
});

test("protected readiness uses management UUIDs and profile slugs without spending inference", async () => {
  const before = observed.length;
  const management = {
    Authorization: `Bearer ${app.token}`,
    "Content-Type": "application/json",
  };
  const state = await fetch(`${app.origin}/api/state`, { headers: management });
  assert.equal(state.status, 200);
  const projected = (await state.json()).profiles.find(
    (profile: Profile) => profile.id === primary.id,
  );
  assert.equal(projected.slug, primary.slug);
  assert.equal(projected.endpoint, app.endpoint(primary.id));
  assert.equal(projected.runtime, "running");
  const models = await fetch(
    `${app.origin}/api/profiles/${primary.id}/models`,
    { headers: management },
  );
  assert.equal(models.status, 200);
  assert.ok(
    (await models.json()).some((entry: { id: string }) => entry.id === model),
  );
  const catalog = await fetch(`${app.endpoint(primary.id)}/models`, {
    headers: { "x-api-key": app.store.secret(`${primary.id}:client`) },
  });
  assert.equal(catalog.status, 200);
  const catalogBody = await catalog.json();
  assert.equal(catalogBody.object, "list");
  assert.ok(
    catalogBody.data.some((entry: { id: string }) => entry.id === model),
  );
  const checkUrl = `${app.origin}/api/profiles/${primary.id}/agent-check`;
  const check = await fetch(checkUrl, {
    method: "POST",
    headers: management,
    body: "{}",
  });
  assert.equal(check.status, 200);
  assert.deepEqual(await check.json(), { ok: true });
  const managementCannotInfer = await fetch(
    `${app.endpoint(primary.id)}/models`,
    { headers: management },
  );
  assert.equal(managementCannotInfer.status, 401);
  const missing = await fetch(
    `${app.origin}/api/profiles/00000000-0000-4000-8000-000000000000/models`,
    { headers: management },
  );
  assert.equal(missing.status, 404);
  const empty = app.store.createProfile("Empty native fixture", "blue");
  await app.core.start(empty.id);
  const emptyCatalog = await fetch(`${app.endpoint(empty.id)}/models`, {
    headers: headers(empty),
  });
  assert.equal(emptyCatalog.status, 200);
  assert.deepEqual(await emptyCatalog.json(), {
    data: [],
    first_id: "",
    has_more: false,
    last_id: "",
  });
  const emptyCheck = await fetch(
    `${app.origin}/api/profiles/${empty.id}/agent-check`,
    { method: "POST", headers: management, body: "{}" },
  );
  assert.equal(emptyCheck.status, 400);
  assert.equal(
    (await emptyCheck.json()).error.message,
    "No models are available. Refresh the profile catalog.",
  );
  await app.core.stop(empty.id);
  const stoppedCatalog = await fetch(`${app.endpoint(empty.id)}/models`, {
    headers: headers(empty),
  });
  assert.equal(stoppedCatalog.status, 503);
  const stoppedCheck = await fetch(
    `${app.origin}/api/profiles/${empty.id}/agent-check`,
    { method: "POST", headers: management, body: "{}" },
  );
  assert.equal(stoppedCheck.status, 502);
  assert.equal(observed.length, before);
});

test("native Messages SSE preserves ordering, tool IDs, fragmented arguments and tool-result replay", async () => {
  const response = await send({
    stream: true,
    messages: message("tools"),
    tools: [
      {
        name: "read_fixture",
        input_schema: {
          type: "object",
          properties: { path: { type: "string" } },
          required: ["path"],
        },
      },
    ],
  });
  assert.equal(response.status, 200);
  assert.match(
    response.headers.get("content-type") ?? "",
    /^text\/event-stream/,
  );
  assert.equal(response.headers.get("x-nonstopvibin-profile"), primary.slug);
  assert.equal(response.headers.get("access-control-allow-origin"), null);
  assert.equal(response.headers.get("set-cookie"), null);
  const received = events(await response.text());
  assert.deepEqual(
    received,
    events(toolStream()),
    "native events, including usage and terminal order, must survive unchanged",
  );
  const blocks = received
    .filter((event) => event.type === "content_block_start")
    .map((event) => {
      const block = record(event.content_block);
      const deltas = received
        .filter(
          (delta) =>
            delta.type === "content_block_delta" && delta.index === event.index,
        )
        .map((event) => {
          const delta = record(event.delta);
          return delta.partial_json;
        });
      return { ...block, input: JSON.parse(deltas.join("")) };
    });
  assert.deepEqual(
    blocks.map((block) => block.input),
    inputs,
  );
  const results = inputs.map((_, index) => ({
    type: "tool_result",
    tool_use_id: `toolu_native_${index}`,
    content: [{ type: "text", text: `result-${index}: π\nquoted "data"` }],
    is_error: index === 1,
  }));
  const messages = [
    ...message("tools"),
    { role: "assistant", content: blocks },
    { role: "user", content: results },
  ];
  const replay = await send({ messages });
  assert.equal(replay.status, 200);
  assert.equal(
    (await replay.json()).content[0].text,
    "fixture replay accepted",
  );
  assert.deepEqual(
    observed.at(-1)?.body.messages,
    [
      ...messages.slice(0, -1),
      {
        role: "user",
        content: [
          results[0],
          { ...results[1], cache_control: { type: "ephemeral" } },
        ],
      },
    ],
    "the core adds an ephemeral cache breakpoint, but must preserve every replay field",
  );
  assert.equal(observed.at(-1)?.path, "/v1/messages?beta=true");
});

test("native image blocks preserve source type, media type and decoded bytes", async () => {
  const messages = [
    {
      role: "user",
      content: [{ type: "text", text: "Inspect fixture image" }, image],
    },
  ];
  const response = await send({ messages });
  assert.equal(response.status, 200);
  await response.text();
  const forwarded = observed.at(-1)?.body.messages[0].content;
  assert.deepEqual(
    Buffer.from(forwarded[1].source.data, "base64"),
    Buffer.from(image.source.data, "base64"),
  );
  assert.deepEqual(forwarded, [
    messages[0].content[0],
    { ...image, cache_control: { type: "ephemeral" } },
  ]);
});

test("native API headers preserve version and extension betas, default version, and replace client auth", async () => {
  const customHeaders = {
    ...headers(),
    "anthropic-version": "2023-01-01",
    "anthropic-beta":
      "message-threads-2026-08-12,mid-conversation-tool-changes-2026-07-01",
    "x-api-key": app.store.secret(`${primary.id}:client`),
    Cookie: "guest=not-upstream",
    "X-Forwarded-For": "192.0.2.1",
  };
  const response = await send(
    { messages: message("headers"), betas: ["fixture-extension-2026-01-01"] },
    primary,
    "messages",
    customHeaders,
  );
  assert.equal(response.status, 200);
  await response.text();
  const request = observed.at(-1)!;
  assert.equal(request.headers["anthropic-version"], "2023-01-01");
  assert.deepEqual(String(request.headers["anthropic-beta"]).split(","), [
    "message-threads-2026-08-12",
    "mid-conversation-tool-changes-2026-07-01",
    "fixture-extension-2026-01-01",
  ]);
  assert.equal(
    request.body.betas,
    undefined,
    "core lifts body betas into the upstream header",
  );
  assert.equal(
    request.headers.authorization,
    `Bearer synthetic-native-${primary.slug}`,
  );
  assert.equal(request.headers["x-api-key"], undefined);
  assert.equal(request.headers.cookie, undefined);
  assert.equal(request.headers["x-forwarded-for"], undefined);
  const { "anthropic-version": version, ...noVersion } = headers();
  assert.equal(version, "2023-06-01");
  const defaulted = await send(
    { messages: message("default headers") },
    primary,
    "messages",
    noVersion,
  );
  assert.equal(defaulted.status, 200);
  await defaulted.text();
  assert.equal(observed.at(-1)?.headers["anthropic-version"], "2023-06-01");
  assert.equal(observed.at(-1)?.headers["anthropic-beta"], undefined);
  const gated = await send(
    { messages: message("disabled thinking"), thinking: { type: "disabled" } },
    primary,
    "messages",
    {
      ...headers(),
      "anthropic-beta": "effort-2025-11-24,message-threads-2026-08-12",
    },
  );
  assert.equal(gated.status, 200);
  await gated.text();
  assert.equal(
    observed.at(-1)?.headers["anthropic-beta"],
    "message-threads-2026-08-12",
    "pinned core filters effort when thinking is disabled, not arbitrary extension betas",
  );
});

test("custom-origin count_tokens is a local text estimate, ignores image bytes, and validates messages", async () => {
  const before = observed.length;
  const textOnly = await send(
    {
      messages: [
        { role: "user", content: [{ type: "text", text: "Count fixture" }] },
      ],
    },
    primary,
    "messages/count_tokens",
  );
  assert.equal(textOnly.status, 200);
  const estimate = await textOnly.json();
  assert.deepEqual(Object.keys(estimate), ["input_tokens"]);
  assert.ok(
    Number.isInteger(estimate.input_tokens) && estimate.input_tokens > 0,
  );
  const withImage = await send(
    {
      messages: [
        {
          role: "user",
          content: [{ type: "text", text: "Count fixture" }, image],
        },
      ],
    },
    primary,
    "messages/count_tokens",
  );
  assert.equal(withImage.status, 200);
  assert.deepEqual(
    await withImage.json(),
    estimate,
    "pinned custom-origin estimator omits image tokens; this is NOT an authoritative multimodal count",
  );
  const invalid = await send(
    { messages: [] },
    primary,
    "messages/count_tokens",
  );
  assert.equal(invalid.status, 400);
  assert.equal((await invalid.json()).error.type, "invalid_request_error");
  assert.equal(
    observed.length,
    before,
    "custom count_tokens must not invoke provider inference or a pretend provider count endpoint",
  );
});

test("native provider errors retain status and type/message but the pinned core drops request_id", async () => {
  const response = await send({ messages: message("reject") });
  assert.equal(response.status, 400);
  assert.match(
    response.headers.get("content-type") ?? "",
    /^application\/json/,
  );
  assert.deepEqual(await response.json(), {
    type: "error",
    error: {
      type: "invalid_request_error",
      message: "synthetic invalid request",
    },
  });
});

test("pinned native clean EOF is silently truncated; explicit error events survive and neither is a success terminal", async () => {
  for (const label of ["truncated", "stream-error"]) {
    const response = await send({ stream: true, messages: message(label) });
    assert.equal(
      response.status,
      200,
      "HTTP status is already committed after message_start",
    );
    const received = events(await response.text());
    assert.equal(received[0].type, "message_start");
    assert.equal(
      received.some((event) => event.type === "message_stop"),
      false,
    );
    // Pinned native-stream branch returns on a clean scanner EOF without
    // validating completion. Consumers MUST require message_stop, not HTTP 200.
    assert.deepEqual(
      received,
      label === "truncated"
        ? events(start)
        : events(
            start +
              frame("error", {
                error: {
                  type: "overloaded_error",
                  message: "synthetic stream failure",
                },
              }),
          ),
    );
  }
});

async function assertTransportReleased() {
  const deadline = Date.now() + 2000;
  const pending = () =>
    heldConnections !== 0 || app.gateway.active.get(primary.id) !== 0;
  while (Date.now() < deadline && pending()) await delay(10);
  assert.deepEqual(
    {
      active: app.gateway.active.get(primary.id),
      providerTransports: heldConnections,
    },
    { active: 0, providerTransports: 0 },
    "local slot and transport must close; this is not proof remote compute or billing stopped",
  );
}
const holdBody = () =>
  JSON.stringify({
    model,
    max_tokens: 64,
    stream: true,
    messages: message("hold"),
  });
for (const teardown of ["graceful FIN", "explicit TCP reset"] as const)
  test(`${teardown} releases the profile slot and idle provider transport within two seconds`, async () => {
    await new Promise<void>((resolve, reject) => {
      const body = JSON.stringify({
        model,
        max_tokens: 64,
        stream: true,
        messages: message("hold"),
      });
      const socket = connect(app.port, "127.0.0.1");
      let wire = "";
      socket.once("error", reject);
      socket.setTimeout(10000, () =>
        socket.destroy(new Error("fixture stream timed out")),
      );
      socket.once("connect", () =>
        socket.write(
          `POST /p/${primary.slug}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:${app.port}\r\nAuthorization: ${headers().Authorization}\r\nContent-Type: application/json\r\nContent-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`,
        ),
      );
      socket.on("data", (chunk) => {
        wire += chunk.toString();
        if (!wire.includes("event: message_start")) return;
        assert.match(wire, /^HTTP\/1\.1 200 /);
        assert.equal(heldConnections, 1);
        assert.equal(app.gateway.active.get(primary.id), 1);
        // Consume the first frame before closing; the provider then remains idle.
        if (teardown === "graceful FIN") socket.destroy();
        else socket.resetAndDestroy();
      });
      socket.once("close", () => {
        assert.ok(wire.includes("event: message_start"));
        resolve();
      });
    });
    await assertTransportReleased();
  });

test("Fetch abort after consuming the first native frame closes idle upstream transport", async () => {
  const controller = new AbortController();
  const response = await fetch(`${app.endpoint(primary.id)}/messages`, {
    method: "POST",
    headers: headers(),
    body: holdBody(),
    signal: controller.signal,
  });
  assert.equal(response.status, 200);
  assert.ok(response.body);
  const reader = response.body.getReader();
  assert.equal((await reader.read()).done, false);
  assert.equal(heldConnections, 1);
  controller.abort();
  reader.releaseLock();
  await assertTransportReleased();
});

test("ClientRequest destroy after consuming the first native frame closes idle upstream transport", async () => {
  await new Promise<void>((resolve, reject) => {
    const request = http.request(
      `${app.endpoint(primary.id)}/messages`,
      {
        method: "POST",
        headers: headers(),
      },
      (response) => {
        assert.equal(response.statusCode, 200);
        response.once("data", () => {
          assert.equal(heldConnections, 1);
          request.destroy();
          resolve();
        });
      },
    );
    request.once("error", reject);
    request.setTimeout(10000, () =>
      request.destroy(new Error("fixture stream timed out")),
    );
    request.end(holdBody());
  });
  await assertTransportReleased();
});

test("completed fragmented bodies preserve keep-alive and reauthenticate each profile on the same connection", async () => {
  const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
  const sockets: http.IncomingMessage["socket"][] = [];
  const before = observed.length;
  try {
    for (const [index, profile] of [primary, other, primary].entries()) {
      const content = [
        { type: "text", text: `${profile.slug}: ${"quoted π ".repeat(4096)}` },
        image,
      ];
      const body = JSON.stringify({
        model,
        max_tokens: 64,
        messages: [{ role: "user", content }],
      });
      const wire = await new Promise<string>((resolve, reject) => {
        const request = http.request(
          `${app.endpoint(profile.id)}/messages`,
          {
            agent,
            method: "POST",
            headers: {
              ...headers(profile),
              "Content-Length": Buffer.byteLength(body),
            },
          },
          (response) => {
            assert.equal(response.statusCode, 200);
            const chunks: Buffer[] = [];
            response.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
            response.once("end", () =>
              resolve(Buffer.concat(chunks).toString()),
            );
            response.once("error", reject);
          },
        );
        request.once("socket", (socket) => sockets.push(socket));
        request.once("error", reject);
        request.setTimeout(10000, () =>
          request.destroy(new Error("keep-alive fixture timed out")),
        );
        request.write(body.slice(0, 37));
        request.write(body.slice(37, 8192));
        request.end(body.slice(8192));
      });
      assert.equal(JSON.parse(wire).stop_reason, "end_turn");
      const request = observed[before + index];
      assert.equal(
        request.headers.authorization,
        `Bearer synthetic-native-${profile.slug}`,
      );
      assert.deepEqual(request.body.messages, [
        {
          role: "user",
          content: [
            content[0],
            { ...image, cache_control: { type: "ephemeral" } },
          ],
        },
      ]);
    }
    assert.equal(sockets.length, 3);
    assert.ok(
      sockets.every((socket) => socket === sockets[0]),
      "next requests must reuse the same gateway TCP connection without profile affinity leakage",
    );
    assert.equal(observed.length, before + 3);
  } finally {
    agent.destroy();
  }
});

test("native profile keys cannot manage, cross profiles or fall back to a healthy pool on exhaustion", async () => {
  const before = observed.length;
  for (const path of [
    "/api/state",
    `/api/profiles/${primary.id}/key`,
    `/api/profiles/${primary.id}/start`,
  ]) {
    const response = await fetch(`${app.origin}${path}`, {
      method: path.endsWith("start") ? "POST" : "GET",
      headers: headers(),
    });
    assert.equal(response.status, 401);
    assert.equal((await response.json()).error.type, "nonstopvibin_error");
  }
  const missingModel = await send({
    model: "synthetic-unavailable-model",
    messages: message("missing model"),
  });
  assert.equal(missingModel.status, 400);
  assert.equal((await missingModel.json()).error.type, "invalid_request_error");
  const wrongProfile = await send(
    { messages: message("wrong profile") },
    primary,
    "messages",
    headers(other),
  );
  assert.equal(wrongProfile.status, 403);
  const wrongAuth = await send(
    { messages: message("conflicting keys") },
    primary,
    "messages",
    { ...headers(), "x-api-key": app.store.secret(`${other.id}:client`) },
  );
  assert.equal(wrongAuth.status, 401);
  assert.equal(observed.length, before);
  const quota = await send({ messages: message("quota") }, exhausted);
  assert.equal(quota.status, 429);
  const quotaBody = await quota.json();
  assert.equal(quotaBody.error.type, "rate_limit_error");
  assert.ok(
    observed
      .slice(before)
      .every(
        (request) =>
          request.headers.authorization ===
          `Bearer synthetic-native-${exhausted.slug}`,
      ),
  );
  const healthy = await send({ messages: message("healthy") }, other);
  assert.equal(healthy.status, 200);
  await healthy.text();
  assert.equal(
    observed.at(-1)?.headers.authorization,
    `Bearer synthetic-native-${other.slug}`,
  );
});
