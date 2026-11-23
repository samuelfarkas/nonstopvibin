// Runs the JavaScript the daemon writes for pi and OpenCode. The sources come from
// tests/golden/agents.json, which the Rust agent setup must reproduce byte for byte.
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createHash } from "node:crypto";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { runInNewContext } from "node:vm";
import cases from "./golden/agents.json" with { type: "json" };

type Json = unknown;
interface Step {
  changes?: Record<string, { content?: string } | null>;
}

const execute = promisify(execFile);
const profileId = "11111111-1111-4111-8111-111111111111";
const providerID = "nonstopvibin-work";

function golden(name: string) {
  const found = cases.find((entry) => entry.name === name);
  assert.ok(found, `golden case ${name}`);
  return found.output as Json;
}

/** The content a scenario's first step wrote to `path`, with the golden's placeholders filled. */
function written(
  scenario: string,
  path: string,
  places: Record<string, string>,
): string {
  const [first] = golden(scenario) as Step[];
  const content = first.changes?.[path]?.content;
  assert.ok(content, `${scenario} writes ${path}`);
  return Object.entries(places).reduce(
    (text, [placeholder, value]) => text.replaceAll(placeholder, value),
    content,
  );
}

interface Bridge {
  key: string;
  port: number;
  status: number;
}

/** The daemon's agent socket: answers `status` with the profile's key and port. */
async function bridge(socket: string, state: Bridge) {
  let requests = 0;
  const server = http.createServer((request, response) => {
    requests++;
    assert.equal(request.url, `/profiles/${profileId}`);
    assert.equal(request.headers.host, "localhost");
    assert.equal(request.method, "GET");
    response.writeHead(state.status).end(`${state.key}\n${state.port}`);
  });
  await new Promise<void>((resolve) => server.listen(socket, resolve));
  return {
    requests: () => requests,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

/** A pi setup on disk: the extension, its key helper and a running bridge. */
async function piSetup() {
  const directory = await realpath(await mkdtemp("/tmp/nv-pi-"));
  const home = join(directory, "home");
  const data = join(directory, "data");
  const places = {
    "<home>": home,
    "<data>": data,
    "<socket-dir>": directory,
  };
  const extension = join(home, ".pi/agent/extensions/nonstopvibin-work.js");
  const helper = join(data, "agent-connections/work/pi-key-4318");
  for (const [path, golden] of [
    [extension, "<home>/.pi/agent/extensions/nonstopvibin-work.js"],
    [helper, "<data>/agent-connections/work/pi-key-4318"],
  ]) {
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, written("scenario pi lifecycle", golden, places));
  }
  await chmod(helper, 0o700);
  const state = { key: `nv_${"A".repeat(43)}`, port: 4318, status: 200 };
  const server = await bridge(join(directory, "agents.sock"), state);
  return {
    directory,
    home,
    extension,
    helper,
    state,
    close: async () => {
      await server.close();
      await rm(directory, { recursive: true, force: true });
    },
  };
}

test("Pi keeps profile controls usable when credentials, discovery or registration fail", async () => {
  const f = await piSetup();
  try {
    const source = (await readFile(f.extension, "utf8"))
      .replace(/^import .*;$/gm, "")
      .replace("export default async function", "async function extension");
    for (const failure of [
      "offline",
      "deleted",
      "empty",
      "malformed",
      "registration",
      "stopped",
      "missing-helper",
    ]) {
      if (failure === "stopped") f.state.status = 409;
      if (failure === "missing-helper") await rm(f.helper);
      const notices: string[] = [];
      const handlers: Array<(...args: unknown[]) => void> = [];
      const commands: string[] = [];
      const removed: string[] = [];
      let registrations = 0;
      const result: Promise<void> = runInNewContext(
        `${source}\nextension(pi)`,
        {
          execFile,
          promisify,
          AbortSignal,
          getBuiltinModel: () => undefined,
          fetch: async () => {
            if (failure === "offline") throw new Error("offline");
            return Response.json(
              {
                models:
                  failure === "empty"
                    ? []
                    : failure === "malformed"
                      ? null
                      : [{ id: "fixture", api: "openai-completions" }],
              },
              { status: failure === "deleted" ? 401 : 200 },
            );
          },
          pi: {
            registerProvider: () => {
              registrations++;
              throw new Error("invalid registration");
            },
            unregisterProvider: (id: string) => removed.push(id),
            events: { emit() {}, on() {} },
            on: (name: string, handler: (...args: unknown[]) => void) => {
              if (name === "session_start") handlers.push(handler);
            },
            registerCommand: (name: string) => commands.push(name),
          },
        },
      );
      await result;
      assert.equal(registrations, failure === "registration" ? 1 : 0, failure);
      assert.deepEqual(commands, ["nv"], failure);
      await handlers[0](
        {},
        { ui: { notify: (text: string) => notices.push(text) } },
      );
      assert.deepEqual(removed, [providerID], failure);
      assert.match(notices[0], /reconnect.*\/reload.*\/nv/);
      assert.equal(notices[0].includes("nv_"), false);
    }
  } finally {
    await f.close();
  }
});

test("pi registers each discovered model with its own protocol, route and live key helper", async () => {
  const f = await piSetup();
  try {
    const selected = `vendor/mo'del"$(touch should-not-exist)__NV_BASE____NV_KEY__{env:UNTRUSTED}{file:/fixture-do-not-read}`;
    // Stand in for pi's bundled catalog, which the loader aliases for extensions.
    const stub = join(f.directory, "node_modules", "@earendil-works", "pi-ai");
    await mkdir(join(stub, "providers"), { recursive: true });
    await writeFile(
      join(stub, "package.json"),
      JSON.stringify({
        name: "@earendil-works/pi-ai",
        type: "module",
        exports: { "./providers/*": "./providers/*.js" },
      }),
    );
    await writeFile(
      join(stub, "providers", "all.js"),
      `export const getBuiltinModel = (provider, id) => provider === "anthropic" && id === ${JSON.stringify(selected)} ? { id, name: "Fixture Opus", provider: "anthropic", baseUrl: "https://api.anthropic.com", headers: { "x-vendor": "1" }, thinkingLevelMap: { max: "max" }, compat: { supportsStrictTools: true, supportsMidConvoSystemMessages: true, supportsMidConvoToolChanges: true }, cost: { input: 9, output: 9, cacheRead: 1, cacheWrite: 1 } } : undefined;`,
    );
    const script = `globalThis.fetch = async (url, init) => { if (!url.endsWith("/p/work/v1/models?nonstopvibin=pi") || init.headers.Authorization !== ${JSON.stringify(`Bearer ${f.state.key}`)} || init.redirect !== "error") throw new Error("wrong discovery scope"); return Response.json({models: ${JSON.stringify([selected, "second-fixture"].map((id, index) => ({ id, name: `${id} · API list price`, api: index ? "openai-completions" : "anthropic-messages", input: ["text"], contextWindow: 64000, maxTokens: 8000, cost: { input: 1, output: 2 } })))} }); }; const {default: extension} = await import(${JSON.stringify(pathToFileURL(f.extension).href)}); await extension({registerProvider:(id,provider)=>console.log(JSON.stringify({id,provider})),events:{emit(){},on(){}},on(){},registerCommand(){}});`;
    const registered = JSON.parse(
      (
        await execute(process.execPath, ["-e", script], {
          cwd: f.directory,
          env: { PATH: process.env.PATH, HOME: f.home },
        })
      ).stdout,
    );
    assert.equal(registered.id, providerID);
    const provider = registered.provider;
    assert.equal(provider.api, undefined, "each model names its own protocol");
    const [known, unknown] = provider.models;
    assert.equal(known.id, selected);
    assert.equal(known.api, "anthropic-messages");
    assert.equal(known.baseUrl, "http://127.0.0.1:4318/p/work");
    assert.equal(
      known.name,
      "Fixture Opus",
      "pi's catalog wins for models it knows",
    );
    assert.deepEqual(known.thinkingLevelMap, { max: "max" });
    assert.deepEqual(known.compat, {
      supportsStrictTools: true,
      supportsMidConvoSystemMessages: true,
      supportsMidConvoEffort: false,
      supportsMidConvoToolChanges: true,
      sendSessionAffinityHeaders: true,
    });
    assert.equal(known.cost.input, 9);
    assert.equal(known.provider, undefined);
    assert.equal(known.headers, undefined);
    assert.equal(unknown.id, "second-fixture");
    assert.equal(unknown.api, "openai-completions");
    assert.equal(
      unknown.compat.sendSessionAffinityHeaders,
      true,
      "pi sends its session id under a name the core recognises",
    );
    assert.equal(unknown.baseUrl, "http://127.0.0.1:4318/p/work/v1");
    assert.equal(unknown.name, "second-fixture · API list price");
    assert.deepEqual(unknown.input, ["text"]);
    assert.equal(unknown.contextWindow, 64000);
    assert.equal(unknown.cost.input, 1);
    assert.equal(unknown.maxTokens, 8000);
    assert.equal(provider.apiKey[0], "!");
    assert.equal(
      (
        await execute("/bin/sh", ["-c", provider.apiKey.slice(1)], {
          cwd: f.directory,
        })
      ).stdout.trim(),
      f.state.key,
    );
  } finally {
    await f.close();
  }
});

test("pi profile controls preserve session routing across selection, restore, failures and concurrent sessions", async () => {
  const fixture = await readFile(
    new URL("./fixtures/pi-profile-controls.mjs", import.meta.url),
    "utf8",
  );
  const directory = await realpath(
    await mkdtemp(join(tmpdir(), "nv-pi-controls-")),
  );
  try {
    const source = join(directory, "controls.mjs");
    await writeFile(
      source,
      `${golden("pi profile controls source") as string}\nexport { connectProfileControls };`,
    );
    const { connectProfileControls } = await import(pathToFileURL(source).href);
    const result: Promise<void> = runInNewContext(
      `const verify = ${fixture}\nverify(assert, connectProfileControls, directory, helpers)`,
      {
        assert,
        connectProfileControls,
        directory,
        helpers: { createHash, mkdir, writeFile },
      },
    );
    await result;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

// The subset of OpenCode's provider config the generated plugin reads and writes.
interface OpenCodeProvider {
  npm: string;
  options?: { fetch: typeof fetch; apiKey?: string };
  models: Record<string, Json>;
}
interface OpenCodeConfig {
  provider: Record<string, OpenCodeProvider>;
  small_model: string;
}
/** Output slot the small_model hook fills with the chosen provider model. */
interface SmallModelOutput {
  model?: Json;
}

test("OpenCode native plugin keeps live credentials and background requests in their profile", async () => {
  const directory = await realpath(await mkdtemp("/tmp/nv-opencode-"));
  const state = { key: `nv_${"A".repeat(43)}`, port: 4318, status: 200 };
  const server = await bridge(join(directory, "agents.sock"), state);
  const originalFetch = globalThis.fetch;
  let requests = 0;
  let redirect = false;
  globalThis.fetch = Object.assign(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith("/models?nonstopvibin=opencode")) {
        assert.equal(
          new Headers(init?.headers).get("Authorization"),
          `Bearer ${state.key}`,
        );
        return Response.json({
          models: [
            {
              id: "vendor/{file:no-read}__model",
              name: "First",
              limit: { context: 64000, output: 8000 },
              cost: { input: 1, output: 2 },
            },
            {
              id: "second-model",
              name: "Second",
              limit: { context: 128000, output: 16000 },
              cost: { input: 3, output: 4 },
            },
          ],
        });
      }
      requests++;
      assert.equal(
        String(input instanceof Request ? input.url : input),
        "http://127.0.0.1:4318/p/work/v1/chat/completions",
      );
      assert.equal(
        new Headers(init?.headers).get("Authorization"),
        `Bearer ${state.key}`,
      );
      assert.equal(new Headers(init?.headers).get("x-fixture"), "preserved");
      assert.equal(init?.redirect, "error");
      return new Response("fixture", { status: redirect ? 302 : 200 });
    },
    originalFetch,
  );
  try {
    const code = written(
      "scenario opencode lifecycle",
      `<home>/.config/opencode/plugins/nonstopvibin-${profileId}.js`,
      { "<socket-dir>": directory },
    );
    const path = join(directory, "plugin.mjs");
    await writeFile(path, code);
    const plugin = await (await import(pathToFileURL(path).href)).default();
    const existing = { npm: "existing", models: {} };
    const config: OpenCodeConfig = {
      provider: { unrelated: existing },
      small_model: "unrelated/model",
    };
    await plugin.config(config);
    assert.equal(config.provider.unrelated, existing);
    assert.equal(config.small_model, "unrelated/model");
    const provider = config.provider[providerID];
    assert.deepEqual(Object.keys(provider.models), [
      "vendor/{file:no-read}__model",
      "second-model",
    ]);
    assert.ok(provider.options);
    assert.equal(provider.options.apiKey, undefined);
    await assert.rejects(plugin.config(config), /conflicting/);
    const endpoint = "http://127.0.0.1:4318/p/work/v1/chat/completions";
    const init = {
      method: "POST",
      headers: { "x-fixture": "preserved", Authorization: "Bearer old" },
      body: "{}",
    };
    await provider.options.fetch(endpoint, init);
    state.key = `nv_${"B".repeat(43)}`;
    await provider.options.fetch(
      new Request(endpoint, { headers: init.headers }),
      { method: "POST" },
    );
    assert.equal(requests, 2);
    assert.equal(server.requests(), 3);
    for (const destination of [
      endpoint.replace("work", "personal"),
      endpoint.replace("4318", "4319"),
      endpoint.replace("127.0.0.1", "example.invalid"),
      endpoint + "?redirect=1",
      endpoint.replace("chat/completions", "../../api/state"),
    ]) {
      await assert.rejects(
        provider.options.fetch(destination, init),
        /outside this profile/,
      );
    }
    assert.equal(
      server.requests(),
      3,
      "destinations are checked before credentials are requested",
    );
    state.port = 4319;
    await assert.rejects(
      provider.options.fetch(endpoint, init),
      /port changed/,
    );
    state.port = 4318;
    state.key = "malformed";
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    state.key = `nv_${"C".repeat(43)}`;
    state.status = 409;
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    assert.equal(requests, 2);
    state.status = 200;
    redirect = true;
    await assert.rejects(provider.options.fetch(endpoint, init), /redirect/);
    const user = { model: { providerID } };
    await assert.rejects(
      plugin["chat.params"]({
        message: user,
        model: { providerID: "personal" },
      }),
      /background model/,
    );
    await plugin["chat.params"]({ message: user, model: { providerID } });
    await plugin["chat.params"]({
      message: { model: { providerID: "unrelated" } },
      model: { providerID: "unrelated" },
    });
    const background: SmallModelOutput = {};
    await plugin["experimental.provider.small_model"](
      { provider: { id: providerID, models: provider.models } },
      background,
    );
    assert.equal(
      background.model,
      provider.models["vendor/{file:no-read}__model"],
    );
    await server.close();
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    assert.equal(requests, 3);
  } finally {
    globalThis.fetch = originalFetch;
    await server.close();
    await rm(directory, { recursive: true, force: true });
  }
});
