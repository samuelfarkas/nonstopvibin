import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { Application } from "../src/server/server.ts";
import { record } from "../src/server/json.ts";
import { CorePool } from "../src/server/core.ts";
import type { JsonObject } from "../src/shared/types.ts";

const args = [{ path: "one.txt" }, { path: 'two "quoted" π.txt' }];
const schema = {
  type: "object",
  properties: { path: { type: "string" } },
  required: ["path"],
};
// Synthetic envelopes matching the core's documented transport-shape validators;
// no provider-issued signatures or credentials are used.
const modelBytes = Buffer.from("claude-sonnet-4-6");
const channel = Buffer.concat([
  Buffer.from([8, 12, 16, 2, 50, modelBytes.length]),
  modelBytes,
]);
const container = Buffer.concat([Buffer.from([10, channel.length]), channel]);
const signature = Buffer.concat([
  Buffer.from([18, container.length]),
  container,
  Buffer.from([24, 1]),
]).toString("base64");
const fernet = Buffer.alloc(73);
fernet[0] = 0x80;
for (let i = 9; i < fernet.length; i++) fernet[i] = i;
const encrypted = fernet.toString("base64url");
const frame = (type: string, data: JsonObject = {}) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`;
const events = (text: string) =>
  text
    .split("\n")
    .filter((line) => line.startsWith("data: {"))
    .map((line) => JSON.parse(line.slice(6)));

function responsesStream(model: string) {
  const reasoning = {
    id: "rs_fixture",
    type: "reasoning",
    summary: [{ type: "summary_text", text: "Check both files." }],
    encrypted_content: encrypted,
  };
  const tools = args.map((input, i) => ({
    id: `fc_${i}`,
    type: "function_call",
    call_id: `call_${i}`,
    name: "read_fixture",
    arguments: JSON.stringify(input),
    status: "completed",
  }));
  let text = frame("response.created", {
    response: {
      id: "resp_fixture",
      object: "response",
      status: "in_progress",
      model,
      output: [],
    },
  });
  text += frame("response.output_item.added", {
    output_index: 0,
    item: { ...reasoning, summary: [] },
  });
  text += frame("response.reasoning_summary_text.delta", {
    item_id: reasoning.id,
    output_index: 0,
    summary_index: 0,
    delta: "Check both files.",
  });
  text += frame("response.output_item.done", {
    output_index: 0,
    item: reasoning,
  });
  for (const [i, tool] of tools.entries()) {
    text += frame("response.output_item.added", {
      output_index: i + 1,
      item: { ...tool, arguments: "", status: "in_progress" },
    });
    for (const delta of [tool.arguments.slice(0, 9), tool.arguments.slice(9)])
      text += frame("response.function_call_arguments.delta", {
        output_index: i + 1,
        item_id: tool.id,
        delta,
      });
    text += frame("response.function_call_arguments.done", {
      output_index: i + 1,
      item_id: tool.id,
      arguments: tool.arguments,
    });
    text += frame("response.output_item.done", {
      output_index: i + 1,
      item: tool,
    });
  }
  return (
    text +
    frame("response.completed", {
      response: {
        id: "resp_fixture",
        object: "response",
        status: "completed",
        model,
        output: [reasoning, ...tools],
        usage: { input_tokens: 12, output_tokens: 8, total_tokens: 20 },
      },
    })
  );
}

function messagesStream(model: string) {
  let text = frame("message_start", {
    message: {
      id: "msg_fixture",
      type: "message",
      role: "assistant",
      model,
      content: [],
      stop_reason: null,
      usage: { input_tokens: 12, output_tokens: 0 },
    },
  });
  text += frame("content_block_start", {
    index: 0,
    content_block: { type: "thinking", thinking: "" },
  });
  text += frame("content_block_delta", {
    index: 0,
    delta: { type: "thinking_delta", thinking: "Check both files." },
  });
  text += frame("content_block_delta", {
    index: 0,
    delta: { type: "signature_delta", signature },
  });
  text += frame("content_block_stop", { index: 0 });
  for (const [i, input] of args.entries()) {
    text += frame("content_block_start", {
      index: i + 1,
      content_block: {
        type: "tool_use",
        id: `toolu_${i}`,
        name: "read_fixture",
        input: {},
      },
    });
    const json = JSON.stringify(input);
    for (const partial_json of [json.slice(0, 9), json.slice(9)])
      text += frame("content_block_delta", {
        index: i + 1,
        delta: { type: "input_json_delta", partial_json },
      });
    text += frame("content_block_stop", { index: i + 1 });
  }
  return (
    text +
    frame("message_delta", {
      delta: { stop_reason: "tool_use", stop_sequence: null },
      usage: { output_tokens: 8 },
    }) +
    frame("message_stop")
  );
}

test(
  "pinned core forwards Pi tool changes and betas through the profile gateway",
  { timeout: 30000 },
  async () => {
    const directory = await mkdtemp("/tmp/nv-pi-tools-");
    const requests: {
      body: ReturnType<typeof JSON.parse>;
      headers: http.IncomingHttpHeaders;
    }[] = [];
    const upstream = http.createServer(async (req, res) => {
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(Buffer.from(chunk));
      const body = JSON.parse(Buffer.concat(chunks).toString());
      requests.push({ body, headers: req.headers });
      if (body.stream) {
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.end(messagesStream(body.model));
      } else {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            id: "msg_fixture",
            type: "message",
            role: "assistant",
            model: body.model,
            content: [{ type: "text", text: "ok" }],
            stop_reason: "end_turn",
            usage: { input_tokens: 1, output_tokens: 1 },
          }),
        );
      }
    });
    await new Promise<void>((resolve) =>
      upstream.listen(0, "127.0.0.1", resolve),
    );
    const address = upstream.address();
    assert.ok(address instanceof Object);
    const app = await Application.create({
      directory,
      agentHome: join(directory, "home"),
      binary: resolve(".vendor/core/cli-proxy-api"),
      clientDirectory: resolve("dist/client"),
      port: 0,
    });
    try {
      for (const oauth of [false, true]) {
        const profile = app.store.createProfile(
          oauth ? "CLI fingerprint fixture" : "API fixture",
          "forest",
        );
        const key = "synthetic-anthropic";
        app.store.saveApiAccount(
          profile.id,
          {
            id: profile.id,
            name: "Synthetic Claude",
            provider: "custom",
            baseUrl: `http://127.0.0.1:${address.port}`,
            prefix: "",
            disabled: false,
            models: [{ id: "claude-opus-5-5", protocol: "anthropic" }],
          },
          key,
        );
        await app.core.start(profile.id);
        if (oauth) {
          // Exercise the OAuth wire profile without a token or external account-profile lookup.
          const config = record(
            await app.core.management(profile.id, "/config"),
          );
          const entries = config["claude-api-key"];
          assert.ok(Array.isArray(entries));
          await app.core.management(
            profile.id,
            "/claude-api-key",
            "PUT",
            entries.map((entry) => ({
              ...record(entry),
              "fingerprint-profile": "claude-code-cli",
            })),
          );
        }
        for (const inline of [false, true])
          for (const stream of [false, true]) {
            const response = await fetch(
              `${app.endpoint(profile.id)}/messages`,
              {
                method: "POST",
                headers: {
                  Authorization: `Bearer ${app.store.secret(`${profile.id}:client`)}`,
                  "Content-Type": "application/json",
                  "anthropic-version": "2023-06-01",
                  "anthropic-beta":
                    "mid-conversation-tool-changes-2026-07-01,message-threads-2026-08-12",
                },
                body: JSON.stringify({
                  model: "claude-opus-5-5",
                  max_tokens: 64,
                  stream,
                  tools: [
                    {
                      name: "read_file",
                      input_schema: { type: "object", properties: {} },
                    },
                    {
                      name: "lookup_notes",
                      input_schema: { type: "object", properties: {} },
                      defer_loading: true,
                    },
                  ],
                  messages: [
                    { role: "user", content: "Before tools change" },
                    {
                      role: "assistant",
                      content: [{ type: "text", text: "ok" }],
                    },
                    {
                      role: "system",
                      content: [
                        {
                          type: "tool_removal",
                          tool: { type: "tool_reference", name: "read_file" },
                        },
                        {
                          type: "tool_addition",
                          tool: inline
                            ? {
                                type: "tool_definition",
                                definition: {
                                  name: "lookup_notes",
                                  input_schema: {
                                    type: "object",
                                    properties: {},
                                  },
                                },
                              }
                            : { type: "tool_reference", name: "lookup_notes" },
                        },
                      ],
                    },
                    { role: "user", content: "After tools change" },
                  ],
                }),
                signal: AbortSignal.timeout(10000),
              },
            );
            assert.equal(response.status, 200, await response.clone().text());
            assert.equal(
              response.headers.get("x-nonstopvibin-profile"),
              profile.slug,
            );
            await response.text();
            const request = requests.at(-1)!;
            const betas = String(request.headers["anthropic-beta"]).split(",");
            assert.ok(
              betas.includes("mid-conversation-tool-changes-2026-07-01"),
            );
            assert.ok(betas.includes("message-threads-2026-08-12"));
            assert.equal(betas.includes("oauth-2025-04-20"), oauth);
            if (inline && oauth)
              assert.ok(
                betas.includes("inline-tools-2026-09-15"),
                "OAuth inline definitions need an automatically assembled beta",
              );
            assert.equal(request.headers.authorization, `Bearer ${key}`);
            const tools = request.body.tools;
            const blocks = request.body.messages.flatMap(
              (message: { content: unknown }) =>
                Array.isArray(message.content) ? message.content : [],
            );
            const removal = blocks.find(
              (block: { type: string }) => block.type === "tool_removal",
            );
            const addition = blocks.find(
              (block: { type: string }) => block.type === "tool_addition",
            );
            assert.equal(removal.tool.name, tools[0].name);
            assert.equal(
              inline ? addition.tool.definition.name : addition.tool.name,
              tools[1].name,
            );
            assert.equal(tools[1].defer_loading, true);
            if (oauth) {
              assert.notEqual(
                tools[0].name,
                "read_file",
                "OAuth aliases must reach declarations and removal references together",
              );
              assert.notEqual(
                tools[1].name,
                "lookup_notes",
                "OAuth aliases must reach additions too",
              );
            }
          }
        await app.core.stop(profile.id);
      }
      assert.equal(requests.length, 8);
    } finally {
      await app.close();
      await new Promise<void>((resolve) => {
        upstream.close(() => resolve());
        upstream.closeAllConnections();
      });
      await rm(directory, { recursive: true, force: true });
      await rm(app.agentSetup.socketDirectory, {
        recursive: true,
        force: true,
      });
    }
  },
);

test(
  "pinned core translates both native harness tool streams and replays signed reasoning within the same profile",
  { timeout: 30000 },
  async () => {
    const directory = await mkdtemp("/tmp/nv-cross-harness-");
    const requests: {
      path: string;
      body: ReturnType<typeof JSON.parse>;
      key?: string;
    }[] = [];
    const upstream = http.createServer(async (req, res) => {
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(Buffer.from(chunk));
      const body = JSON.parse(Buffer.concat(chunks).toString());
      const path = req.url ?? "";
      requests.push({
        path,
        body,
        key: req.headers.authorization ?? req.headers["x-api-key"]?.toString(),
      });
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.end(
        path.includes("responses")
          ? responsesStream(body.model)
          : messagesStream(body.model),
      );
    });
    await new Promise<void>((resolve) =>
      upstream.listen(0, "127.0.0.1", resolve),
    );
    const address = upstream.address();
    assert.ok(address instanceof Object);
    const binary = resolve(".vendor/core/cli-proxy-api");
    const localCatalogBinary = join(directory, "core-local-catalog-wrapper");
    // Keep this fixture deterministic while the production core refreshes its
    // mutable remote catalog: the wrapper adds --local-model only here.
    const app = await Application.create({
      directory,
      agentHome: join(directory, "home"),
      binary: localCatalogBinary,
      clientDirectory: resolve("dist/client"),
      port: 0,
    });
    try {
      // Verify the underlying pinned core separately; the wrapper is test-only
      // and catalog-pinned, so Application.create cannot verify it before write.
      await new CorePool(app.store, binary).verifyBinary();
      await writeFile(
        localCatalogBinary,
        `#!/bin/sh\nexec '${binary.replaceAll("'", "'\\''")}' "$@" --local-model\n`,
        { mode: 0o755 },
      );
      await chmod(localCatalogBinary, 0o755);
      const profile = app.store.createProfile("Mixed models", "forest");
      const foreign = app.store.createProfile("Other profile", "blue");
      for (const [id, protocol] of [
        ["gpt-6.1-sol", "responses"],
        ["claude-sonnet-4-6", "anthropic"],
      ] as const)
        app.store.saveApiAccount(
          profile.id,
          {
            id: protocol,
            name: protocol,
            provider: "custom",
            baseUrl: `http://127.0.0.1:${address.port}/v1`,
            prefix: "",
            disabled: false,
            models: [{ id, protocol }],
          },
          `synthetic-${protocol}`,
        );
      await app.core.start(profile.id);
      const endpoint = app.endpoint(profile.id);
      const headers = {
        Authorization: `Bearer ${app.store.secret(`${profile.id}:client`)}`,
        "Content-Type": "application/json",
        "anthropic-version": "2023-06-01",
      };
      async function send(path: string, body: JsonObject) {
        const response = await fetch(`${endpoint}/${path}`, {
          method: "POST",
          headers,
          body: JSON.stringify(body),
          signal: AbortSignal.timeout(10000),
        });
        assert.equal(response.status, 200, await response.clone().text());
        return events(await response.text());
      }
      const catalog = await fetch(`${endpoint}/models?client_version=0.140.0`, {
        headers,
      });
      assert.equal(catalog.status, 200);
      const models = (await catalog.json()).models;
      assert.deepEqual(
        models.map((model: { slug: string }) => model.slug).sort(),
        ["claude-sonnet-4-6", "gpt-6.1-sol"],
      );
      for (const model of models)
        assert.ok(Array.isArray(model.supported_reasoning_levels));

      const claudeRequest = {
        model: "gpt-6.1-sol",
        stream: true,
        max_tokens: 4096,
        thinking: { type: "adaptive" },
        output_config: { effort: "high" },
        messages: [{ role: "user", content: "Read both files." }],
        tools: [
          {
            name: "read_fixture",
            description: "Read a synthetic fixture",
            input_schema: schema,
          },
        ],
      };
      const claudeEvents = await send("messages", claudeRequest);
      const blocks = claudeEvents
        .filter((event) => event.type === "content_block_start")
        .map((start) => {
          const block = { ...start.content_block };
          const deltas = claudeEvents
            .filter(
              (event) =>
                event.type === "content_block_delta" &&
                event.index === start.index,
            )
            .map((event) => event.delta);
          if (block.type === "tool_use")
            block.input = JSON.parse(
              deltas.map((delta) => delta.partial_json ?? "").join(""),
            );
          if (block.type === "thinking") {
            block.thinking = deltas
              .map((delta) => delta.thinking ?? "")
              .join("");
            block.signature = deltas
              .map((delta) => delta.signature ?? "")
              .join("");
          }
          return block;
        });
      const calls = blocks.filter((block) => block.type === "tool_use");
      assert.deepEqual(
        calls.map((call) => call.input),
        args,
      );
      assert.ok(calls.every((call) => call.name === "read_fixture"));
      assert.equal(new Set(calls.map((call) => call.id)).size, 2);
      assert.equal(claudeEvents.at(-1).type, "message_stop");
      assert.equal(
        claudeEvents.find((event) => event.type === "message_delta").delta
          .stop_reason,
        "tool_use",
      );
      assert.equal(requests[0].body.reasoning.effort, "high");
      assert.deepEqual(requests[0].body.tools[0].parameters, schema);
      await send("messages", {
        ...claudeRequest,
        messages: [
          ...claudeRequest.messages,
          { role: "assistant", content: blocks },
          {
            role: "user",
            content: calls.map((call, i) => ({
              type: "tool_result",
              tool_use_id: call.id,
              content: `fixture-result-${i}`,
            })),
          },
        ],
      });
      const replay = requests[1].body.input;
      assert.deepEqual(
        replay
          .filter(
            (item: { type: string }) => item.type === "function_call_output",
          )
          .map((item: { call_id: string; output: string }) => [
            item.call_id,
            item.output,
          ]),
        calls.map((call, i) => [call.id, `fixture-result-${i}`]),
      );
      assert.equal(
        replay.find((item: { type: string }) => item.type === "reasoning")
          .encrypted_content,
        encrypted,
      );

      const codexRequest = {
        model: "claude-sonnet-4-6",
        stream: true,
        reasoning: { effort: "high", summary: "auto" },
        input: [{ role: "user", content: "Read both files." }],
        tools: [
          {
            type: "function",
            name: "read_fixture",
            description: "Read a synthetic fixture",
            parameters: schema,
          },
        ],
      };
      const codexEvents = await send("responses", codexRequest);
      const output = codexEvents.find(
        (event) => event.type === "response.completed",
      ).response.output;
      const functions = output.filter(
        (item: { type: string }) => item.type === "function_call",
      );
      assert.deepEqual(
        functions.map((item: { arguments: string }) =>
          JSON.parse(item.arguments),
        ),
        args,
      );
      assert.ok(
        functions.every(
          (item: { name: string }) => item.name === "read_fixture",
        ),
      );
      assert.equal(requests[2].body.thinking.type, "adaptive");
      assert.equal(requests[2].body.output_config.effort, "high");
      assert.deepEqual(requests[2].body.tools[0].input_schema, schema);
      await send("responses", {
        ...codexRequest,
        input: [
          ...codexRequest.input,
          ...output,
          ...functions.map((item: { call_id: string }, i: number) => ({
            type: "function_call_output",
            call_id: item.call_id,
            output: `fixture-result-${i}`,
          })),
        ],
      });
      const content = requests[3].body.messages.flatMap(
        (message: { content: unknown[] }) =>
          Array.isArray(message.content) ? message.content : [],
      );
      assert.equal(
        content.find((block: { type: string }) => block.type === "thinking")
          .signature,
        signature,
      );
      assert.deepEqual(
        content
          .filter((block: { type: string }) => block.type === "tool_result")
          .map((block: { tool_use_id: string; content: string }) => [
            block.tool_use_id,
            block.content,
          ]),
        functions.map((item: { call_id: string }, i: number) => [
          item.call_id,
          `fixture-result-${i}`,
        ]),
      );
      assert.ok(
        requests
          .slice(0, 2)
          .every((request) => request.key === "Bearer synthetic-responses"),
      );
      assert.ok(
        requests
          .slice(2)
          .every(
            (request) =>
              request.key === "Bearer synthetic-anthropic" ||
              request.key === "synthetic-anthropic",
          ),
      );
      const denied = await fetch(`${endpoint}/responses`, {
        method: "POST",
        headers: {
          ...headers,
          Authorization: `Bearer ${app.store.secret(`${foreign.id}:client`)}`,
        },
        body: JSON.stringify(codexRequest),
      });
      assert.equal(denied.status, 403);
      assert.equal(requests.length, 4);
    } finally {
      await app.close();
      await new Promise<void>((resolve) => {
        upstream.close(() => resolve());
        upstream.closeAllConnections();
      });
      await rm(directory, { recursive: true, force: true });
      await rm(app.agentSetup.socketDirectory, {
        recursive: true,
        force: true,
      });
    }
  },
);
