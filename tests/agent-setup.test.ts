import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { randomUUID } from "node:crypto";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  readdir,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { runInNewContext } from "node:vm";
import { AgentSetup, agentSetupSchema } from "../src/server/agent-setup.ts";
import {
  nativeConfiguration,
  nativeProvider,
  shellQuote,
} from "../src/server/agent-config.ts";
import {
  mergeNativeConfig,
  projectDirectory,
  readConfig,
} from "../src/server/native-config-files.ts";
import type { Agent, AgentSetupInput, Profile } from "../src/shared/types.ts";

const execute = promisify(execFile);
const agents: Agent[] = ["pi", "codex", "opencode", "claude"];
function profile(slug = "work"): Profile {
  return {
    id: randomUUID(),
    slug,
    name: "Work's $(touch should-not-exist)",
    color: "forest",
    strategy: "round-robin",
    sessionAffinity: false,
    enabled: true,
    createdAt: "2026-09-06",
  };
}
function input(
  agent: Agent,
  project: string,
  model = "fixture-model",
): [AgentSetupInput, string[]] {
  return [
    { agent, ...(agent === "claude" && { projectDirectory: project }) },
    [model, "second-fixture"],
  ];
}
async function fixture() {
  const directory = await realpath(await mkdtemp("/tmp/nv-setup-"));
  const company = profile();
  const personal = profile("personal");
  const keys = new Map([
    [company.id, `nv_${"A".repeat(43)}`],
    [personal.id, `nv_${"B".repeat(43)}`],
  ]);
  let port = 4318;
  const connection = (id: string) => {
    const key = keys.get(id);
    if (!key) throw new Error("Stopped or unknown profile");
    return { key, port };
  };
  const home = join(directory, "home");
  const project = join(directory, "project's $dollar");
  await mkdir(home);
  await mkdir(project);
  const setup = new AgentSetup(directory, connection, { home });
  const run = (path: string) =>
    execute(path, [], {
      cwd: project,
      env: { PATH: "/usr/bin:/bin", HOME: home },
      timeout: 10_000,
    });
  return {
    directory,
    company,
    personal,
    keys,
    setup,
    connection,
    project,
    home,
    run,
    setPort: (next: number) => {
      port = next;
    },
    close: async () => {
      await setup.close();
      await rm(directory, { recursive: true, force: true });
      await rm(setup.socketDirectory, { recursive: true, force: true });
    },
  };
}

test("Pi keeps profile controls usable when credentials, discovery or registration fail", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("pi", f.project),
    );
    const source = (await readFile(installed.files[0], "utf8"))
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
      if (failure === "stopped") f.keys.delete(f.company.id);
      if (failure === "missing-helper") await rm(installed.files[2]);
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
      assert.deepEqual(removed, [installed.provider], failure);
      assert.match(notices[0], /reconnect.*\/reload.*\/nv/);
      assert.equal(notices[0].includes("nv_"), false);
    }
  } finally {
    await f.close();
  }
});

test(
  "custom Codex roots reject shared write access",
  { skip: process.getuid === undefined },
  async () => {
    const directory = await realpath(await mkdtemp("/tmp/nv-codex-root-"));
    const home = join(directory, "home");
    const codex = join(directory, "codex");
    await mkdir(home);
    await mkdir(codex);
    await chmod(codex, 0o777);
    const setup = new AgentSetup(
      join(directory, "data"),
      () => ({ key: `nv_${"A".repeat(43)}`, port: 4399 }),
      { home, codex },
    );
    try {
      await assert.rejects(
        setup.install(profile(), ...input("codex", directory)),
        /directory you own, without symbolic links or shared write access/,
      );
    } finally {
      await setup.close();
      await rm(directory, { recursive: true, force: true });
      await rm(setup.socketDirectory, { recursive: true, force: true });
    }
  },
);

test("native installs use harness configuration, keep secrets out of files and resolve immutable profile keys", async () => {
  const f = await fixture();
  try {
    for (const agent of agents) {
      const selected = `vendor/mo'del"$(touch should-not-exist)__NV_BASE____NV_KEY__{env:UNTRUSTED}{file:/fixture-do-not-read}`;
      const installed = await f.setup.install(
        f.company,
        ...input(agent, f.project, selected),
      );
      assert.deepEqual(await f.setup.status(f.company, agent), installed);
      assert.equal(installed.provider, nativeProvider(f.company));
      assert.equal(
        installed.command,
        agent === "codex"
          ? "codex --profile nonstopvibin-work"
          : agent === "claude"
            ? `claude --settings ${shellQuote(installed.files[3])}`
            : agent,
      );
      for (const path of installed.files) {
        const content = await readFile(path, "utf8");
        for (const key of f.keys.values())
          assert.equal(content.includes(key), false);
        assert.equal((await stat(path)).mode & 0o077, 0);
      }
      if (agent !== "opencode") {
        const helper = installed.files[2];
        assert.equal((await stat(helper)).mode & 0o700, 0o700);
        await execute("/bin/sh", ["-n", helper]);
        assert.equal(
          (await f.run(helper)).stdout.trim(),
          f.keys.get(f.company.id),
        );
      }
      const content = await readFile(installed.files[0], "utf8");
      if (agent === "codex") {
        // SAFETY: the generated config.toml is this app's own output; the shape
        // asserted here is the one nativeConfiguration writes for Codex.
        const parsed = JSON.parse(
          (
            await execute(process.execPath, [
              "-e",
              "console.log(JSON.stringify(Bun.TOML.parse(process.argv[1])))",
              content,
            ])
          ).stdout,
        ) as {
          model: string;
          model_providers: Record<
            string,
            {
              base_url: string;
              auth: { command: string };
              env_key?: string;
              requires_openai_auth?: boolean;
            }
          >;
        };
        assert.equal(parsed.model, undefined);
        const provider = parsed.model_providers[installed.provider];
        assert.equal(provider.base_url, "http://127.0.0.1:4318/p/work/v1");
        assert.equal(provider.auth.command, installed.files[2]);
        assert.equal(provider.env_key, undefined);
        assert.equal(provider.requires_openai_auth, undefined);
      } else if (agent === "claude") {
        const settings = JSON.parse(content);
        const launch = JSON.parse(await readFile(installed.files[3], "utf8"));
        assert.deepEqual(launch.modelPicker, {
          options: [selected, "second-fixture"].map((model) => ({ model })),
          replaceBuiltInOptions: true,
        });
        assert.equal(
          settings.modelPicker,
          undefined,
          "project settings ignore the picker",
        );
        assert.equal(launch.model, undefined, "the agent owns model choice");
        assert.deepEqual(launch.env, settings.env);
        assert.equal(launch.apiKeyHelper, settings.apiKeyHelper);
        assert.equal(settings.env.ANTHROPIC_MODEL, undefined);
        assert.equal(
          settings.env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY,
          "1",
        );
        assert.equal(
          settings.env.ANTHROPIC_BASE_URL,
          "http://127.0.0.1:4318/p/work",
        );
        assert.equal(settings.env.ANTHROPIC_AUTH_TOKEN, "");
        assert.equal(settings.env.ANTHROPIC_API_KEY, "");
        assert.equal(settings.env.ANTHROPIC_DEFAULT_HAIKU_MODEL, undefined);
        assert.equal(settings.env.CLAUDE_CODE_SUBAGENT_MODEL, undefined);
        assert.equal(settings.permissions, undefined);
        assert.equal(
          (
            await execute("/bin/sh", ["-c", settings.apiKeyHelper], {
              cwd: f.project,
            })
          ).stdout.trim(),
          f.keys.get(f.company.id),
        );
      } else if (agent === "pi") {
        const descriptor = JSON.parse(
          await readFile(installed.files[1], "utf8"),
        );
        assert.equal(
          descriptor.content,
          content,
          "the installed extension must match its generated descriptor",
        );
        // Stand in for pi's bundled catalog, which the loader aliases for extensions.
        const stub = join(
          f.directory,
          "node_modules",
          "@earendil-works",
          "pi-ai",
        );
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
          `export const getBuiltinModel = (provider, id) => provider === "anthropic" && id === ${JSON.stringify(selected)} ? { id, name: "Fixture Opus", provider: "anthropic", baseUrl: "https://api.anthropic.com", headers: { "x-vendor": "1" }, thinkingLevelMap: { max: "max" }, compat: { supportsStrictTools: true }, cost: { input: 9, output: 9, cacheRead: 1, cacheWrite: 1 } } : undefined;`,
        );
        const script = `globalThis.fetch = async (url, init) => { if (!url.endsWith("/p/work/v1/models?nonstopvibin=pi") || init.headers.Authorization !== ${JSON.stringify(`Bearer ${f.keys.get(f.company.id)}`)} || init.redirect !== "error") throw new Error("wrong discovery scope"); return Response.json({models: ${JSON.stringify([selected, "second-fixture"].map((id, index) => ({ id, name: `${id} · API list price`, api: index ? "openai-completions" : "anthropic-messages", input: ["text"], contextWindow: 64000, maxTokens: 8000, cost: { input: 1, output: 2 } })))} }); }; const {default: extension} = await import(${JSON.stringify(pathToFileURL(installed.files[0]).href)}); await extension({registerProvider:(id,provider)=>console.log(JSON.stringify({id,provider})),events:{emit(){},on(){}},on(){},registerCommand(){}});`;
        const registered = JSON.parse(
          (await execute(process.execPath, ["-e", script])).stdout,
        );
        assert.equal(registered.id, installed.provider);
        const provider = registered.provider;
        assert.equal(
          provider.api,
          undefined,
          "each model names its own protocol",
        );
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
          supportsMidConvoEffort: false,
        });
        assert.equal(known.cost.input, 9);
        assert.equal(known.provider, undefined);
        assert.equal(known.headers, undefined);
        assert.equal(unknown.id, "second-fixture");
        assert.equal(unknown.api, "openai-completions");
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
              cwd: f.project,
            })
          ).stdout.trim(),
          f.keys.get(f.company.id),
        );
      } else {
        assert.ok(installed.files[0].endsWith(".js"));
        assert.ok(content.includes("experimental.provider.small_model"));
      }
    }
    assert.deepEqual(
      await readdir(f.project),
      [".claude"],
      "catalog/profile strings must never execute shell code",
    );
    const other = await f.setup.install(
      f.personal,
      ...input("codex", f.project),
    );
    const work = await f.setup.status(f.company, "codex");
    assert.ok(work);
    const [workKey, personalKey] = await Promise.all([
      f.run(work.files[2]),
      f.run(other.files[2]),
    ]);
    assert.notEqual(workKey.stdout, personalKey.stdout);
    f.keys.set(f.personal.id, `nv_${"C".repeat(43)}`);
    assert.equal(
      (await f.run(other.files[2])).stdout.trim(),
      f.keys.get(f.personal.id),
    );
    f.setPort(54321);
    assert.equal(
      (await f.setup.status(f.personal, "codex"))?.needsReconnect,
      true,
    );
    await assert.rejects(f.run(other.files[2]), /gateway port changed/);
    const updated = await f.setup.install(
      f.personal,
      ...input("codex", f.project),
    );
    assert.equal(
      (await f.setup.status(f.personal, "codex"))?.needsReconnect,
      undefined,
    );
    assert.equal(
      (await f.run(updated.files[2])).stdout.trim(),
      f.keys.get(f.personal.id),
    );
    await assert.rejects(
      f.run(other.files[2]),
      /gateway port changed/,
      "old sessions must retain an old-port helper and fail closed",
    );
  } finally {
    await f.close();
  }
});

test("native credential helpers fail closed when stopped and resume through the same private broker", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("pi", f.project),
    );
    f.keys.delete(f.company.id);
    await assert.rejects(
      f.run(installed.files[2]),
      /Open nonstopvibin and start/,
    );
    f.keys.set(f.company.id, `nv_${"A".repeat(43)}`);
    await f.setup.close();
    await assert.rejects(
      f.run(installed.files[2]),
      /Open nonstopvibin and start/,
    );
    const restarted = new AgentSetup(f.directory, f.connection, {
      home: f.home,
    });
    try {
      await restarted.restore();
      assert.equal(
        (await f.run(installed.files[2])).stdout.trim(),
        f.keys.get(f.company.id),
      );
    } finally {
      await restarted.close();
    }
  } finally {
    await f.close();
  }
});

test("the credential bridge only serves its narrow local socket protocol", async () => {
  const f = await fixture();
  try {
    await f.setup.install(f.company, ...input("codex", f.project));
    assert.equal((await stat(f.setup.socketDirectory)).mode & 0o077, 0);
    assert.equal((await stat(f.setup.socketPath)).mode & 0o777, 0o600);
    function request(path: string, method = "GET", headers = {}) {
      return new Promise<{ status: number | undefined; body: string }>(
        (resolve, reject) => {
          const request = http.request(
            {
              socketPath: f.setup.socketPath,
              path,
              method,
              headers: { Host: "localhost", ...headers },
            },
            (response) => {
              let body = "";
              response.on("data", (chunk) => {
                body += chunk;
              });
              response.on("end", () =>
                resolve({ status: response.statusCode, body }),
              );
            },
          );
          request.on("error", reject);
          request.end();
        },
      );
    }
    const valid = await request(`/profiles/${f.company.id}`);
    assert.equal(valid.status, 200);
    assert.equal(valid.body, `${f.keys.get(f.company.id)}\n4318`);
    for (const [path, method, headers] of [
      ["/api/state", "GET", {}],
      [`/profiles/${f.company.id}`, "POST", {}],
      [`/profiles/${f.company.id}?extra=true`, "GET", {}],
      [`/profiles/${f.company.id}`, "GET", { Origin: "https://example.com" }],
      [`/profiles/${f.company.id}`, "GET", { Host: "evil.example" }],
    ] as const) {
      const response = await request(path, method, headers);
      assert.equal(response.status, 403);
      assert.equal(response.body.includes("nv_"), false);
    }
    assert.equal((await request(`/profiles/${randomUUID()}`)).status, 409);
    const second = new AgentSetup(f.directory, f.connection, { home: f.home });
    await assert.rejects(second.restore(), /Another nonstopvibin instance/);
    assert.equal((await request(`/profiles/${f.company.id}`)).status, 200);
  } finally {
    await f.close();
  }
});

test("native update and disconnect preserve unrelated preferences and refuse edits, symlinks and unsafe inputs", async () => {
  const f = await fixture();
  try {
    await mkdir(join(f.home, ".pi", "agent"), { recursive: true });
    const piFile = join(f.home, ".pi", "agent", "models.json");
    const existing = {
      providers: {
        other: {
          api: "openai-completions",
          apiKey: "synthetic-existing",
          models: [{ id: "other-model" }],
        },
      },
      userPreference: true,
    };
    const existingPi =
      "// Existing commented configuration\n" + JSON.stringify(existing);
    await writeFile(piFile, existingPi);
    await mkdir(join(f.project, ".claude"));
    const claudeFile = join(f.project, ".claude", "settings.local.json");
    const claudeExisting = {
      permissions: { deny: ["Bash(rm *)"] },
      env: {
        USER_PREFERENCE: "keep",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
        ANTHROPIC_MODEL: "fixture-model",
      },
    };
    await writeFile(claudeFile, JSON.stringify(claudeExisting));
    for (const agent of ["pi", "claude"] as const) {
      const installed = await f.setup.install(
        f.company,
        ...input(agent, f.project),
      );
      if (agent === "pi") {
        const manifest = JSON.parse(await readFile(installed.files[1], "utf8"));
        assert.equal(manifest.version, 6);
        const legacy = join(
          f.home,
          ".pi",
          "agent",
          "extensions",
          `nonstopvibin-${f.company.id}.js`,
        );
        await writeFile(legacy, await readFile(installed.files[0], "utf8"));
        await rm(installed.files[0]);
        manifest.version = 4;
        await writeFile(installed.files[1], JSON.stringify(manifest));
        assert.equal(
          (await f.setup.status(f.company, agent))?.needsReconnect,
          true,
        );
      }
      await f.setup.install(f.company, ...input(agent, f.project, "new-model"));
      assert.equal(
        (await f.setup.status(f.company, agent))?.needsReconnect,
        undefined,
      );
      assert.deepEqual((await f.setup.status(f.company, agent))?.models, [
        "new-model",
        "second-fixture",
      ]);
      await f.setup.remove(f.company, agent);
      assert.equal(await f.setup.status(f.company, agent), null);
      if (agent === "pi") {
        await assert.rejects(stat(installed.files[0]), { code: "ENOENT" });
        assert.equal(await readFile(piFile, "utf8"), existingPi);
      } else
        assert.deepEqual(
          JSON.parse(await readFile(installed.files[0], "utf8")),
          claudeExisting,
        );
      for (const file of installed.files.slice(1))
        await assert.rejects(stat(file), { code: "ENOENT" });
    }
    const installed = await f.setup.install(
      f.company,
      ...input("pi", f.project),
    );
    const edited =
      (await readFile(installed.files[0], "utf8")) + "\n// User edit\n";
    await writeFile(installed.files[0], edited);
    await assert.rejects(f.setup.remove(f.company, "pi"), /edited outside/);
    await assert.rejects(
      f.setup.install(f.company, ...input("pi", f.project)),
      /edited outside/,
    );
    assert.equal(await readFile(installed.files[0], "utf8"), edited);
    await rm(installed.files[0]);
    await symlink(claudeFile, installed.files[0]);
    await assert.rejects(
      f.setup.install(f.company, ...input("pi", f.project)),
      /symbolic link/,
    );
    assert.deepEqual(
      JSON.parse(await readFile(claudeFile, "utf8")),
      claudeExisting,
    );
    const escape = profile("escape");
    await symlink(f.directory, join(f.setup.directory, escape.slug));
    await assert.rejects(
      f.setup.install(escape, ...input("codex", f.project)),
      /without symbolic links/,
    );
    for (const invalid of [
      { ...input("pi", f.project)[0], maxTokens: 40000 },
      { ...input("pi", f.project)[0], model: "bad\nmodel" },
      { ...input("pi", f.project)[0], agent: "../../outside" },
      { ...input("pi", f.project)[0], directory: "/elsewhere" },
      { ...input("claude", f.project)[0], projectDirectory: "relative" },
      { ...input("pi", f.project)[0], projectDirectory: f.project },
    ])
      assert.equal(agentSetupSchema.safeParse(invalid).success, false);
  } finally {
    await f.close();
  }
});

test("pi readable extension migration preserves edits and supports manual removal", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("pi", f.project),
    );
    const readable = join(
      f.home,
      ".pi",
      "agent",
      "extensions",
      "nonstopvibin-work.js",
    );
    const legacy = join(
      f.home,
      ".pi",
      "agent",
      "extensions",
      `nonstopvibin-${f.company.id}.js`,
    );
    assert.equal(installed.files[0], readable);
    const content = await readFile(readable, "utf8");
    const manifest = JSON.parse(await readFile(installed.files[1], "utf8"));
    manifest.version = 5;
    await writeFile(installed.files[1], JSON.stringify(manifest));
    await rm(readable);
    await writeFile(legacy, content + "\n// user edit");
    await assert.rejects(
      f.setup.install(f.company, ...input("pi", f.project)),
      /edited outside/,
    );
    await assert.rejects(stat(readable), { code: "ENOENT" });
    await writeFile(legacy, content);
    await writeFile(readable, "// unrelated extension");
    await assert.rejects(
      f.setup.install(f.company, ...input("pi", f.project)),
      /edited outside/,
    );
    assert.equal(await readFile(legacy, "utf8"), content);
    await rm(readable);
    await f.setup.install(f.company, ...input("pi", f.project));
    await assert.rejects(stat(legacy), { code: "ENOENT" });
    await rm(readable); // Deleting the obvious extension file is a supported uninstall.
    assert.equal((await f.setup.status(f.company, "pi"))?.needsReconnect, true);
    await f.setup.remove(f.company, "pi");
    assert.equal(await f.setup.status(f.company, "pi"), null);
  } finally {
    await f.close();
  }
});

test("native merge rejects occupied settings and malformed JSON; special files cannot block reads", async () => {
  const original = JSON.stringify({
    apiKeyHelper: "my-existing-helper",
    env: { KEEP: "value" },
  });
  assert.throws(
    () =>
      mergeNativeConfig(
        "claude",
        original,
        undefined,
        JSON.stringify({ apiKeyHelper: "new-helper", env: {} }),
      ),
    /already exists/,
  );
  assert.throws(
    () => mergeNativeConfig("claude", "[]", undefined, "{}"),
    /valid JSON object/,
  );
  assert.throws(
    () =>
      mergeNativeConfig("codex", "user config", undefined, "generated config"),
    /already exists/,
  );
  const directory = await mkdtemp("/tmp/nv-fifo-");
  try {
    const fifo = join(directory, "settings.json");
    await execute("/usr/bin/mkfifo", [fifo]);
    await assert.rejects(readConfig(fifo), /ordinary file/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("missing native files can be repaired and Claude project boundaries are explicit", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("codex", f.project),
    );
    await rm(installed.files[0]);
    assert.equal(
      (await f.setup.status(f.company, "codex"))?.needsReconnect,
      true,
    );
    await f.setup.install(f.company, ...input("codex", f.project));
    assert.equal(
      (await f.setup.status(f.company, "codex"))?.needsReconnect,
      undefined,
    );
    const git = join(f.project, ".git");
    await mkdir(git);
    const nested = join(f.project, "src");
    await mkdir(nested);
    assert.equal(await projectDirectory(f.project), f.project);
    await assert.rejects(projectDirectory(nested), /repository root/);
    const dotted = join(f.project, "..nested");
    await mkdir(dotted);
    await assert.rejects(projectDirectory(dotted), /repository root/);
    const worktree = join(f.directory, "worktree");
    const metadata = join(git, "worktrees", "test");
    await mkdir(worktree);
    await mkdir(metadata, { recursive: true });
    await writeFile(join(worktree, ".git"), `gitdir: ${metadata}\n`);
    await writeFile(join(metadata, "commondir"), "../..\n");
    await assert.rejects(projectDirectory(worktree), /Git worktree/);
    await f.setup.install(f.company, ...input("claude", f.project));
    await assert.rejects(
      f.setup.install(f.personal, ...input("claude", f.project)),
      /already exists/,
    );
    const second = await f.setup.install(
      f.company,
      ...input("claude", f.home, "second-model"),
    );
    assert.deepEqual(second.projects, [f.home, f.project].sort());
    assert.deepEqual(
      (await f.setup.status(f.company, "claude", f.project))?.models,
      ["fixture-model", "second-fixture"],
    );
    assert.deepEqual(
      (await f.setup.status(f.company, "claude", f.home))?.models,
      ["second-model", "second-fixture"],
    );
    await assert.rejects(
      f.setup.remove(f.company, "claude"),
      /Choose which Claude project/,
    );
    await f.setup.remove(f.company, "claude", f.project);
    assert.equal(await f.setup.status(f.company, "claude", f.project), null);
    assert.deepEqual(
      (await f.setup.status(f.company, "claude", f.home))?.models,
      ["second-model", "second-fixture"],
    );
    assert.equal(
      (await f.run(second.files[2])).stdout.trim(),
      f.keys.get(f.company.id),
    );
    await f.setup.remove(f.company, "claude", f.home);
    await assert.rejects(stat(second.files[2]), { code: "ENOENT" });
  } finally {
    await f.close();
  }
});

test("a deleted Claude project can be disconnected without hiding the other projects", async () => {
  const f = await fixture();
  try {
    await mkdir(join(f.home, ".claude"));
    await writeFile(
      join(f.home, ".claude", "settings.local.json"),
      JSON.stringify({ env: { ANTHROPIC_MODEL: "fixture-model" } }),
    );
    await f.setup.install(f.company, ...input("claude", f.home));
    const remaining = await f.setup.install(
      f.company,
      ...input("claude", f.project),
    );
    await rm(f.home, { recursive: true });
    const status = await f.setup.status(f.company, "claude");
    assert.equal(status?.projectDirectory, f.home);
    assert.equal(status?.needsReconnect, true);
    assert.deepEqual(status?.projects, [f.home, f.project].sort());
    await f.setup.remove(f.company, "claude", f.home);
    assert.equal(await f.setup.status(f.company, "claude", f.home), null);
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.projectDirectory,
      f.project,
    );
    assert.equal(
      (await f.run(remaining.files[2])).stdout.trim(),
      f.keys.get(f.company.id),
    );
  } finally {
    await f.close();
  }
});

test("reconnecting a legacy single-model setup removes only its owned model overrides", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("claude", f.project),
    );
    const path = installed.files[1];
    const manifest = JSON.parse(await readFile(path, "utf8"));
    const legacy = JSON.parse(manifest.content);
    legacy.env.ANTHROPIC_MODEL = "old-model";
    legacy.env.ANTHROPIC_DEFAULT_HAIKU_MODEL = "old-model";
    legacy.env.CLAUDE_CODE_SUBAGENT_MODEL = "old-model";
    delete legacy.env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY;
    manifest.version = 1;
    manifest.input = {
      ...manifest.input,
      model: "old-model",
      contextWindow: 32768,
      maxTokens: 4096,
    };
    delete manifest.models;
    manifest.content = JSON.stringify(legacy);
    await writeFile(path, JSON.stringify(manifest));
    await writeFile(
      installed.files[0],
      JSON.stringify({
        ...legacy,
        permissions: { deny: ["Bash(rm *)"] },
        model: "user-chosen-model",
      }),
    );
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      true,
    );
    await f.setup.install(f.company, ...input("claude", f.project));
    const settings = JSON.parse(await readFile(installed.files[0], "utf8"));
    assert.equal(settings.env.ANTHROPIC_MODEL, undefined);
    assert.equal(settings.env.ANTHROPIC_DEFAULT_HAIKU_MODEL, undefined);
    assert.equal(settings.env.CLAUDE_CODE_SUBAGENT_MODEL, undefined);
    assert.equal(settings.env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY, "1");
    assert.equal(settings.model, "user-chosen-model");
    assert.deepEqual(settings.permissions, { deny: ["Bash(rm *)"] });
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      undefined,
    );
    await f.setup.remove(f.company, "claude");
    assert.deepEqual(JSON.parse(await readFile(installed.files[0], "utf8")), {
      model: "user-chosen-model",
      permissions: { deny: ["Bash(rm *)"] },
    });
  } finally {
    await f.close();
  }
});

test("Codex runtime state survives status, reconnect and disconnect", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("codex", f.project),
    );
    const connection = await readFile(installed.files[0], "utf8");
    const state =
      '[hooks.state]\n[hooks.state."/fixture/hooks.json:stop:0:0"]\ntrusted_hash = "fixture-hash"\n\n[tui.model_availability_nux]\nfixture-model = true\n';
    await writeFile(installed.files[0], connection + state);
    assert.equal(
      (await f.setup.status(f.company, "codex"))?.needsReconnect,
      undefined,
    );
    f.setPort(4399);
    await f.setup.install(f.company, ...input("codex", f.project));
    const updated = await readFile(installed.files[0], "utf8");
    assert.ok(updated.includes(state));
    assert.ok(updated.includes("127.0.0.1:4399"));
    await writeFile(
      installed.files[0],
      updated.replace("/p/work/v1", "/p/personal/v1"),
    );
    await assert.rejects(f.setup.status(f.company, "codex"), /edited outside/);
    await assert.rejects(f.setup.remove(f.company, "codex"), /edited outside/);
    await writeFile(installed.files[0], updated);
    await f.setup.remove(f.company, "codex");
    assert.equal(
      (await readFile(installed.files[0], "utf8")).trim(),
      state.trim(),
    );
  } finally {
    await f.close();
  }
});

test("Codex native model preferences survive reconnect and disconnect without permitting routing edits", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("codex", f.project),
    );
    const connection = await readFile(installed.files[0], "utf8");
    const preference =
      'model = "second-fixture"\nmodel_reasoning_effort = "high"\n';
    await writeFile(installed.files[0], preference + connection);
    assert.equal(
      (await f.setup.status(f.company, "codex"))?.needsReconnect,
      undefined,
    );
    await f.setup.install(f.company, ...input("codex", f.project));
    assert.equal(
      await readFile(installed.files[0], "utf8"),
      preference + connection,
    );
    await writeFile(
      installed.files[0],
      preference + connection.replace("/p/work/v1", "/p/personal/v1"),
    );
    await assert.rejects(
      f.setup.install(f.company, ...input("codex", f.project)),
      /edited outside/,
    );
    await writeFile(installed.files[0], preference + connection);
    await f.setup.remove(f.company, "codex");
    assert.equal(
      (await readFile(installed.files[0], "utf8")).trim(),
      preference.trim(),
    );
    const legacy = connection.replace(
      "\n\n[",
      '\nmodel = "old-pinned-model"\nreview_model = "old-pinned-model"\n\n[',
    );
    assert.equal(
      mergeNativeConfig("codex", legacy, legacy, connection),
      connection,
    );
  } finally {
    await f.close();
  }
});

test("Claude picker sync repairs missing files, migrates discovery-only setups, and refuses edits without changing the route", async () => {
  const f = await fixture();
  try {
    const installed = await f.setup.install(
      f.company,
      ...input("claude", f.project),
    );
    const picker = installed.files[3];
    await f.setup.install(
      f.company,
      { agent: "claude", projectDirectory: f.project },
      ["new-gpt", "new-claude"],
    );
    const synced = await readFile(picker, "utf8");
    assert.deepEqual(JSON.parse(synced).modelPicker.options, [
      { model: "new-gpt" },
      { model: "new-claude" },
    ]);
    const route = await readFile(installed.files[0], "utf8");
    await writeFile(picker, synced + "\n");
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      true,
    );
    await assert.rejects(
      f.setup.install(f.company, ...input("claude", f.project)),
      /edited outside/,
    );
    await assert.rejects(f.setup.remove(f.company, "claude"), /edited outside/);
    assert.equal(await readFile(installed.files[0], "utf8"), route);
    await rm(picker);
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      true,
    );
    const manifest = JSON.parse(await readFile(installed.files[1], "utf8"));
    manifest.version = 2;
    await writeFile(installed.files[1], JSON.stringify(manifest));
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      true,
    );
    await f.setup.install(f.company, ...input("claude", f.project));
    assert.equal(
      (await f.setup.status(f.company, "claude"))?.needsReconnect,
      undefined,
    );
    await rm(picker);
    await symlink(installed.files[0], picker);
    await assert.rejects(
      f.setup.install(f.company, ...input("claude", f.project)),
      /symbolic link/,
    );
    assert.equal(await readFile(installed.files[0], "utf8"), route);
  } finally {
    await f.close();
  }
});

test("Codex only tags requests for gateway model filtering when other providers are off", () => {
  const toml = (otherProviders?: boolean) =>
    nativeConfiguration(
      profile(),
      { agent: "codex", otherProviders },
      "/tmp/helper",
      4321,
    );
  const tag = 'http_headers = { "x-nonstopvibin-agent" = "codex" }';
  assert.ok(toml(false).includes(`wire_api = "responses"\n${tag}\n`));
  assert.ok(!toml(true).includes("x-nonstopvibin-agent"));
  assert.ok(!toml().includes("x-nonstopvibin-agent"));
  assert.throws(() =>
    agentSetupSchema.parse({ agent: "pi", otherProviders: false }),
  );
});

test("native setup preserves safe slugs produced by older truncation rules", async () => {
  const f = await fixture();
  try {
    for (const slug of ["a".repeat(39) + "-", "a".repeat(39) + "--2"]) {
      const legacy = { ...f.company, slug };
      const result = await f.setup.install(legacy, { agent: "codex" }, [
        "fixture-model",
      ]);
      assert.ok(result.command.includes(slug));
      assert.ok(result.files.some((file) => file.includes(slug)));
      await f.setup.remove(legacy, "codex");
    }
    await assert.rejects(
      f.setup.install({ ...f.company, slug: "../escape" }, { agent: "codex" }, [
        "fixture-model",
      ]),
    );
  } finally {
    await f.close();
  }
});
