// Run: node scripts/check-pi-profiles.mjs /path/to/@earendil-works/pi-coding-agent
// Uses only disposable settings, synthetic credentials and a loopback provider.
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  writeFile,
  rm,
  readFile,
  realpath,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash, randomUUID } from "node:crypto";
import http from "node:http";
import { spawnSync } from "node:child_process";
import ts from "typescript";
import { createRequire } from "node:module";
import { piProfileControls } from "../src/server/pi-profile-controls.ts";
import { nativeConfiguration } from "../src/server/agent-config.ts";

if (!process.argv[2])
  throw new Error("Pass the installed pi-coding-agent package directory.");
if (process.argv[3] !== "--isolated") {
  const child = spawnSync(
    process.execPath,
    [process.argv[1], resolve(process.argv[2]), "--isolated"],
    {
      stdio: "inherit",
      env: {
        PATH: process.env.PATH,
        PI_OFFLINE: "1",
        PI_SKIP_VERSION_CHECK: "1",
      },
      timeout: 90_000,
    },
  );
  if (child.error) throw child.error;
  process.exit(child.status ?? 1);
}

const packageDir = resolve(process.argv[2]);
const {
  createAgentSession,
  AgentSessionRuntime,
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} = await import(pathToFileURL(join(packageDir, "dist/index.js")));
const root = await mkdtemp("/tmp/nv-pi-runtime-");
const agentDir = join(root, "agent");
const extensionDir = join(agentDir, "extensions");
await mkdir(extensionDir, { recursive: true });
const keys = { work: "nv_" + "W".repeat(43), personal: "nv_" + "P".repeat(43) };
const requests = [];
const sessions = [];
let personalAvailable = true;
const model = (id, api = "openai-completions") => ({
  id,
  name: id,
  api,
  reasoning: false,
  input: ["text"],
  cost: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 64000,
  maxTokens: 2048,
});
const catalog = [
  model("shared"),
  model("second"),
  model("anthropic-fixture", "anthropic-messages"),
  ...Array.from({ length: 25 }, (_, index) => model("fixture-" + index)),
];
const server = http.createServer(async (req, res) => {
  const match =
    /^\/p\/(work|personal)(?:\/v1)?\/(models|chat\/completions|messages)(?:\?.*)?$/.exec(
      req.url ?? "",
    );
  if (
    !match ||
    (req.headers.authorization !== "Bearer " + keys[match[1]] &&
      req.headers["x-api-key"] !== keys[match[1]])
  ) {
    res.writeHead(403).end();
    return;
  }
  if (match[1] === "personal" && !personalAvailable) {
    res.writeHead(503).end();
    return;
  }
  if (match[2] === "models") {
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({ models: catalog }));
    return;
  }
  let raw = "";
  for await (const chunk of req) raw += chunk;
  const body = JSON.parse(raw);
  requests.push({ profile: match[1], model: body.model });
  res.setHeader("Content-Type", "text/event-stream");
  if (match[2] === "messages") {
    const emit = (type, data) =>
      res.write(
        `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`,
      );
    emit("message_start", {
      message: {
        id: "synthetic",
        type: "message",
        role: "assistant",
        model: body.model,
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: 1, output_tokens: 0 },
      },
    });
    emit("content_block_start", {
      index: 0,
      content_block: { type: "text", text: "" },
    });
    emit("content_block_delta", {
      index: 0,
      delta: { type: "text_delta", text: "synthetic reply" },
    });
    emit("content_block_stop", { index: 0 });
    emit("message_delta", {
      delta: { stop_reason: "end_turn", stop_sequence: null },
      usage: { output_tokens: 2 },
    });
    emit("message_stop", {});
  } else {
    res.write(
      `data: ${JSON.stringify({ id: "synthetic", object: "chat.completion.chunk", created: 1, model: body.model, choices: [{ index: 0, delta: { role: "assistant", content: "synthetic reply" }, finish_reason: null }] })}\n\n`,
    );
    res.write(
      `data: ${JSON.stringify({ id: "synthetic", object: "chat.completion.chunk", created: 1, model: body.model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 1, completion_tokens: 2, total_tokens: 3 } })}\n\n`,
    );
    res.write("data: [DONE]\n\n");
  }
  res.end();
});
try {
  const controlsPath = join(root, "controls.js");
  await writeFile(controlsPath, piProfileControls);
  const piRequire = createRequire(join(packageDir, "package.json"));
  const program = ts.createProgram([controlsPath], {
    allowJs: true,
    checkJs: true,
    noEmit: true,
    strict: true,
    skipLibCheck: true,
    target: ts.ScriptTarget.ES2023,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    paths: {
      "@earendil-works/pi-coding-agent": [join(packageDir, "dist/index.d.ts")],
      "@earendil-works/pi-tui": [
        piRequire.resolve("@earendil-works/pi-tui").replace(/\.js$/, ".d.ts"),
      ],
    },
  });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  assert.equal(
    diagnostics.length,
    0,
    ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCurrentDirectory: () => root,
      getCanonicalFileName: (name) => name,
      getNewLine: () => "\n",
    }),
  );
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  const port = server.address().port;
  for (const slug of Object.keys(keys)) {
    await mkdir(join(root, "profiles", slug), { recursive: true });
    const helper = join(root, "profiles", slug, "key");
    await writeFile(helper, `#!/bin/sh\nprintf '%s\\n' '${keys[slug]}'\n`, {
      mode: 0o700,
    });
    const profile = {
      id: randomUUID(),
      slug,
      name: slug,
      color: "forest",
      strategy: "round-robin",
      sessionAffinity: false,
      enabled: true,
      createdAt: "2026-09-07",
    };
    await writeFile(
      join(extensionDir, slug + ".js"),
      nativeConfiguration(profile, { agent: "pi" }, helper, port),
      { mode: 0o600 },
    );
  }
  async function openSession(
    slug,
    manager = SessionManager.inMemory(root),
    cwd = root,
    sessionStartEvent,
    launch = { provider: "synthetic-other", modelId: "shared" },
  ) {
    const settingsManager = SettingsManager.inMemory({
      compaction: { enabled: false },
      retry: { enabled: false },
      enableInstallTelemetry: false,
    });
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir,
      settingsManager,
      noSkills: true,
      noContextFiles: true,
      noPromptTemplates: true,
      noThemes: true,
      systemPrompt: "Synthetic transport check.",
    });
    await resourceLoader.reload();
    const modelRuntime = await ModelRuntime.create({
      authPath: join(agentDir, "auth.json"),
      modelsPath: null,
      modelsStorePath: join(root, randomUUID() + ".json"),
      allowModelNetwork: false,
    });
    const result = await createAgentSession({
      cwd,
      agentDir,
      resourceLoader,
      modelRuntime,
      settingsManager,
      sessionManager: manager,
      sessionStartEvent,
      tools: [],
      model: {
        ...catalog.find((model) => model.id === launch.modelId),
        provider: launch.provider,
        baseUrl: `http://127.0.0.1:${port}/p/${launch.provider === "synthetic-other" ? "work" : slug}/v1`,
      },
    });
    assert.equal(
      result.extensionsResult.errors.length,
      0,
      "real pi loads both generated extensions",
    );
    // This registration survives extension reload unless the controls restore it.
    const otherConfig = {
      baseUrl: `http://127.0.0.1:${port}/p/work/v1`,
      apiKey: keys.work,
      models: catalog,
    };
    modelRuntime.registerProvider("synthetic-other", otherConfig);
    const errors = [];
    const notices = [];
    const choices = [];
    let footer;
    const uiContext = {
      custom: async (factory) => {
        let selected;
        let closed = false;
        const bindings = {
          "tui.select.up": "\u001b[A",
          "tui.select.down": "\u001b[B",
          "tui.select.confirm": "\r",
          "tui.select.cancel": "\u001b",
        };
        const component = await factory(
          { requestRender() {} },
          { fg: (_color, text) => text },
          { matches: (data, key) => data === bindings[key] },
          (value) => {
            selected = value;
            closed = true;
          },
        );
        component.focused = true;
        assert.ok(
          component.render(90).length <= 20,
          "picker limits visible rows",
        );
        for (const character of "no-such-fixture")
          component.handleInput(character);
        assert.match(component.render(90).join("\n"), /No matches/);
        component.handleInput("\r");
        assert.equal(
          closed,
          false,
          "enter cannot select a hidden model after an empty search",
        );
        component.handleInput("\u0015"); // Ctrl+U clears the search using pi's input bindings.
        const choice = choices.shift();
        if (choice === undefined) component.handleInput("\u001b");
        else {
          for (const character of choice) component.handleInput(character);
          const rendered = component.render(90).join("\n");
          assert.ok(
            rendered.includes(choice),
            "typed search retains its matching model",
          );
          assert.equal(
            rendered.includes("shared"),
            choice.includes("shared"),
            "search filters unrelated models",
          );
          component.handleInput("\r");
        }
        assert.ok(closed, "enter/escape completes the picker");
        return selected;
      },
      select: async (_title, options) => {
        const choice = choices.shift();
        return typeof choice === "function" ? choice(options) : choice;
      },
      notify: (text) => notices.push(text),
      setStatus: (_name, text) => {
        footer = text;
      },
    };
    await result.session.bindExtensions({
      mode: "tui",
      uiContext,
      onError: (error) => errors.push(error),
    });
    sessions.push(result.session);
    return {
      ...result,
      services: {
        cwd,
        agentDir,
        modelRuntime,
        settingsManager,
        resourceLoader,
        diagnostics: [],
      },
      diagnostics: [],
      modelRuntime,
      choices,
      errors,
      notices,
      manager,
      get footer() {
        return footer;
      },
    };
  }
  assert.equal(spawnSync("git", ["init", "-q", root]).status, 0);
  const nested = join(root, "nested");
  const otherRepo = join(root, "other-repo");
  await mkdir(nested);
  await mkdir(otherRepo);
  assert.equal(spawnSync("git", ["init", "-q", otherRepo]).status, 0);
  const work = await openSession("work", undefined, undefined, undefined, {
    provider: "nonstopvibin-work",
    modelId: "shared",
  });
  const personal = await openSession(
    "personal",
    undefined,
    undefined,
    undefined,
    { provider: "nonstopvibin-personal", modelId: "shared" },
  );
  assert.match(work.footer, /work/);
  assert.match(personal.footer, /personal/);
  const available = (item) =>
    item.modelRuntime.getAvailableSnapshot().map((model) => model.provider);
  assert.deepEqual(
    [...new Set(available(work))],
    ["nonstopvibin-work"],
    "native /model availability contains only the active profile",
  );
  assert.equal(
    new Set(work.modelRuntime.getAvailableSnapshot().map((model) => model.id))
      .size,
    catalog.length,
    "no duplicate models",
  );
  await work.modelRuntime.setRuntimeApiKey("openai", "synthetic-key");
  assert.deepEqual(
    [...new Set(available(work))],
    ["nonstopvibin-work"],
    "authenticating a native provider keeps the active profile catalog isolated",
  );
  await work.modelRuntime.removeRuntimeApiKey("openai");
  await work.session.setModel(
    work.modelRuntime.getModel("nonstopvibin-work", "second"),
  );
  const explicitGpt = await openSession(
    "work",
    undefined,
    undefined,
    undefined,
    { provider: "nonstopvibin-work", modelId: "shared" },
  );
  assert.equal(explicitGpt.session.model.provider, "nonstopvibin-work");
  assert.equal(
    explicitGpt.session.model.id,
    "shared",
    "an explicit nonstopvibin model outranks the repository model",
  );
  const explicitClaude = await openSession(
    "work",
    undefined,
    undefined,
    undefined,
    { provider: "nonstopvibin-work", modelId: "anthropic-fixture" },
  );
  assert.equal(explicitClaude.session.model.provider, "nonstopvibin-work");
  assert.equal(explicitClaude.session.model.id, "anthropic-fixture");
  const explicitPersonal = await openSession(
    "personal",
    undefined,
    undefined,
    undefined,
    { provider: "nonstopvibin-personal", modelId: "anthropic-fixture" },
  );
  assert.equal(
    explicitPersonal.session.model.provider,
    "nonstopvibin-personal",
    "an explicit provider must not switch to the repository profile",
  );
  assert.equal(explicitPersonal.session.model.id, "anthropic-fixture");
  assert.equal(
    spawnSync("git", [
      "-C",
      root,
      "-c",
      "user.name=Synthetic",
      "-c",
      "user.email=fixture@example.invalid",
      "-c",
      "core.hooksPath=/dev/null",
      "commit",
      "--allow-empty",
      "-qm",
      "fixture",
    ]).status,
    0,
  );
  const linked = join(root, "linked");
  assert.equal(
    spawnSync("git", ["-C", root, "worktree", "add", "--detach", linked])
      .status,
    0,
  );
  const linkedNested = join(linked, "nested");
  await mkdir(linkedNested);
  const mainPreference = join(
    root,
    "profiles",
    "pi-preferences",
    createHash("sha256")
      .update(await realpath(root))
      .digest("hex") + ".json",
  );
  const savedMainPreference = await readFile(mainPreference, "utf8");
  await writeFile(
    mainPreference,
    JSON.stringify({ provider: "nonstopvibin-work", modelId: "removed-model" }),
  );
  const missingInheritedModel = await openSession(
    "work",
    SessionManager.inMemory(linkedNested),
    linkedNested,
  );
  const beforeMissingInheritedModel = requests.length;
  await missingInheritedModel.session.prompt(
    "Synthetic missing inherited model request",
  );
  assert.equal(
    requests.length,
    beforeMissingInheritedModel,
    "a missing inherited model must not silently use the same profile's fallback",
  );
  const missingContext = {
    messages: [
      {
        role: "user",
        content: "Synthetic direct missing-model check",
        timestamp: 1,
      },
    ],
  };
  const unavailableProfileModel = missingInheritedModel.modelRuntime.getModel(
    "nonstopvibin-work",
    "shared",
  );
  for (const method of ["stream", "streamSimple"]) {
    const rejected = await missingInheritedModel.modelRuntime[method](
      unavailableProfileModel,
      missingContext,
    ).result();
    assert.equal(rejected.stopReason, "error");
    assert.match(
      rejected.errorMessage,
      /saved nonstopvibin model is unavailable/,
    );
  }
  assert.equal(
    requests.length,
    beforeMissingInheritedModel,
    "direct transports also block unresolved inherited models",
  );
  await missingInheritedModel.session.setModel(
    missingInheritedModel.modelRuntime.getModel("nonstopvibin-work", "second"),
  );
  await missingInheritedModel.session.prompt(
    "Synthetic request after choosing an available model",
  );
  assert.equal(
    requests.length,
    beforeMissingInheritedModel + 1,
    "explicit valid model selection recovers",
  );
  assert.deepEqual(requests.at(-1), { profile: "work", model: "second" });
  await writeFile(mainPreference, savedMainPreference);
  await rm(
    join(
      root,
      "profiles",
      "pi-preferences",
      createHash("sha256")
        .update(await realpath(linked))
        .digest("hex") + ".json",
    ),
  );
  const inherited = await openSession(
    "personal",
    SessionManager.inMemory(linkedNested),
    linkedNested,
  );
  assert.equal(
    inherited.session.model.provider,
    "nonstopvibin-work",
    "new worktree inherits the main checkout profile",
  );
  assert.equal(
    inherited.session.model.id,
    "second",
    "new worktree inherits the main checkout model",
  );
  const linkedPreference = join(
    root,
    "profiles",
    "pi-preferences",
    createHash("sha256")
      .update(await realpath(linked))
      .digest("hex") + ".json",
  );
  await assert.rejects(
    readFile(linkedPreference),
    { code: "ENOENT" },
    "inheritance does not create a redundant preference file",
  );
  const inheritedHost = new AgentSessionRuntime(
    inherited.session,
    inherited.services,
    (options) =>
      openSession(
        "personal",
        options.sessionManager,
        options.cwd,
        options.sessionStartEvent,
      ),
  );
  await inheritedHost.newSession();
  assert.equal(
    inheritedHost.session.model.provider,
    "nonstopvibin-work",
    "/new in a worktree inherits the main profile",
  );
  assert.equal(inheritedHost.session.model.id, "second");
  await inheritedHost.session.prompt("/nv personal");
  const overridden = await openSession(
    "work",
    SessionManager.inMemory(linked),
    linked,
  );
  assert.equal(
    overridden.session.model.provider,
    "nonstopvibin-personal",
    "explicit worktree choice overrides the main checkout",
  );
  const mainUnchanged = await openSession("personal");
  assert.equal(
    mainUnchanged.session.model.provider,
    "nonstopvibin-work",
    "worktree choice does not overwrite the main checkout",
  );
  const resumedInherited = await openSession(
    "work",
    inherited.manager,
    linkedNested,
  );
  assert.equal(
    resumedInherited.session.model.provider,
    "nonstopvibin-work",
    "conversation selection remains authoritative",
  );
  overridden.choices.push("Use other pi providers");
  await overridden.session.prompt("/nv");
  const releasedWorktree = await openSession(
    "work",
    SessionManager.inMemory(linked),
    linked,
  );
  assert.match(
    releasedWorktree.footer,
    /to choose a profile/,
    "explicit worktree release does not inherit the main lock",
  );
  await writeFile(linkedPreference, "invalid JSON");
  const corruptWorktree = await openSession(
    "work",
    SessionManager.inMemory(linked),
    linked,
  );
  assert.match(
    corruptWorktree.footer,
    /unavailable/,
    "corrupt worktree preference does not inherit a different profile",
  );
  const restarted = await openSession(
    "personal",
    SessionManager.inMemory(nested),
    nested,
  );
  assert.equal(
    restarted.session.model.provider,
    "nonstopvibin-work",
    "restart from a repo subdirectory restores the profile",
  );
  assert.equal(
    restarted.session.model.id,
    "second",
    "native model selection is remembered",
  );
  const host = new AgentSessionRuntime(
    restarted.session,
    restarted.services,
    (options) =>
      openSession(
        "personal",
        options.sessionManager,
        options.cwd,
        options.sessionStartEvent,
      ),
  );
  assert.deepEqual(await host.newSession(), { cancelled: false });
  assert.equal(
    host.session.model.provider,
    "nonstopvibin-work",
    "/new keeps the repository profile",
  );
  assert.equal(host.session.model.id, "second");
  const independent = await openSession(
    "personal",
    SessionManager.inMemory(otherRepo),
    otherRepo,
    undefined,
    { provider: "nonstopvibin-personal", modelId: "shared" },
  );
  assert.equal(
    independent.session.model.provider,
    "nonstopvibin-personal",
    "another repo does not inherit the preference",
  );
  const resumedPersonal = await openSession("work", personal.manager);
  assert.equal(
    resumedPersonal.session.model.provider,
    "nonstopvibin-personal",
    "saved conversation beats repository preference",
  );
  const branchSession = await openSession("work");
  const initialModelEntry = branchSession.manager
    .getBranch()
    .find((entry) => entry.type === "model_change");
  assert.ok(initialModelEntry);
  await personal.session.setModel(
    personal.modelRuntime.getModel("nonstopvibin-personal", "second"),
  );
  await branchSession.session.navigateTree(initialModelEntry.id);
  assert.equal(
    branchSession.session.model.provider,
    "nonstopvibin-work",
    "tree navigation restores its recorded profile ahead of another session's repository preference",
  );
  await personal.session.setModel(
    personal.modelRuntime.getModel("nonstopvibin-personal", "shared"),
  );
  await work.session.prompt("Synthetic work request");
  assert.equal(requests.at(-1)?.profile, "work");
  const context = {
    messages: [
      { role: "user", content: "Synthetic boundary check", timestamp: 1 },
    ],
  };
  const foreign = personal.session.model;
  const count = requests.length;
  for (const method of ["stream", "streamSimple"]) {
    const rejected = await work.modelRuntime[method](foreign, context).result();
    assert.equal(rejected.stopReason, "error");
    assert.match(rejected.errorMessage, /Switch profiles with \/nv/);
  }
  assert.equal(
    requests.length,
    count,
    "pi's actual transport sends no wrong-profile request",
  );
  await work.session.setModel(foreign);
  assert.equal(work.session.model.provider, "nonstopvibin-work");
  work.session.setScopedModels([
    { model: foreign },
    { model: work.session.model },
  ]);
  await work.session.cycleModel();
  assert.equal(
    work.session.model.provider,
    "nonstopvibin-work",
    "native Ctrl+P is guarded",
  );
  work.choices.push("anthropic-fixture");
  await work.session.prompt("/nv model");
  assert.equal(work.session.model.id, "anthropic-fixture");
  await work.session.prompt("Synthetic Anthropic request");
  assert.deepEqual(requests.at(-1), {
    profile: "work",
    model: "anthropic-fixture",
  });
  await work.session.prompt("/nv personal");
  assert.equal(work.session.model.provider, "nonstopvibin-personal");
  assert.deepEqual(
    [...new Set(available(work))],
    ["nonstopvibin-personal"],
    "native catalog follows profile switches",
  );
  assert.equal(
    work.session.model.id,
    "anthropic-fixture",
    "profile switch preserves a shared model",
  );
  await personal.session.prompt("Synthetic independent session");
  assert.equal(personal.session.model.id, "shared");
  await work.modelRuntime.refresh({ allowNetwork: false });
  const blocked = await work.modelRuntime
    .streamSimple(
      { ...work.session.model, provider: "nonstopvibin-work", id: "shared" },
      context,
    )
    .result();
  assert.equal(
    blocked.stopReason,
    "error",
    "catalog refresh preserves the guard",
  );
  for (let reload = 0; reload < 2; reload++) {
    await work.session.reload();
    assert.equal(work.session.model.provider, "nonstopvibin-personal");
    work.choices.push("Use other pi providers");
    await work.session.prompt("/nv");
    assert.match(work.footer, /to choose a profile/);
    assert.ok(available(work).includes("synthetic-other"));
    assert.ok(
      !available(work).some((id) => id.startsWith("nonstopvibin-")),
      "release restores native providers without all profile duplicates",
    );
    const released = await openSession("work");
    assert.match(
      released.footer,
      /to choose a profile/,
      "release is remembered for new sessions",
    );
    await work.session.setModel(
      work.modelRuntime.getModel("synthetic-other", "shared"),
    );
    const beforeReloadRequest = requests.length;
    await work.session.prompt("Synthetic request after reload and unlock");
    assert.equal(
      requests.length,
      beforeReloadRequest + 1,
      "unlock after reload reaches the original provider",
    );
    assert.deepEqual(requests.at(-1), { profile: "work", model: "shared" });
    await work.session.prompt("/nv personal");
  }
  const restored = await openSession("work", work.manager);
  assert.equal(restored.session.model.provider, "nonstopvibin-personal");
  assert.equal(restored.session.model.id, "shared");
  personalAvailable = false;
  const freshMissing = await openSession("work");
  assert.match(
    freshMissing.footer,
    /personal/,
    "missing repository preference never falls back to another profile",
  );
  const beforeFreshMissing = requests.length;
  await freshMissing.session.prompt("Synthetic unavailable default request");
  assert.equal(requests.length, beforeFreshMissing);
  const missing = await openSession("work", work.manager);
  const beforeMissing = requests.length;
  await missing.session.prompt("Synthetic missing-profile request");
  assert.equal(
    requests.length,
    beforeMissing,
    "an unavailable restored profile does not fall back to Work",
  );
  assert.match(missing.footer, /personal/);
  await rm(join(root, "profiles", "personal", "key"));
  const deleted = await openSession("work", work.manager);
  await restored.session.reload();
  for (const item of [deleted, restored]) {
    assert.equal(
      item.modelRuntime.getModel("nonstopvibin-personal", "shared"),
      undefined,
    );
    const before = requests.length;
    await item.session.prompt("Synthetic deleted-profile request");
    assert.equal(requests.length, before);
    await item.session.prompt("/nv work");
    assert.equal(item.session.model.provider, "nonstopvibin-work");
  }
  const extensionPath = join(extensionDir, "work.js");
  const extensionContent = await readFile(extensionPath, "utf8");
  await rm(extensionPath);
  await deleted.session.reload();
  assert.equal(
    deleted.modelRuntime.getModel("nonstopvibin-work", "shared"),
    undefined,
    "deleting the extension and /reload removes its provider",
  );
  await writeFile(extensionPath, extensionContent);
  await deleted.session.reload();
  assert.equal(deleted.session.model.provider, "nonstopvibin-work");
  const preference = join(
    root,
    "profiles",
    "pi-preferences",
    createHash("sha256")
      .update(await realpath(root))
      .digest("hex") + ".json",
  );
  await writeFile(preference, "invalid JSON");
  const corrupt = await openSession("work");
  assert.match(corrupt.footer, /unavailable/);
  const beforeCorrupt = requests.length;
  await corrupt.session.prompt("Synthetic corrupt preference request");
  assert.equal(
    requests.length,
    beforeCorrupt,
    "corrupt preference fails closed",
  );
  for (const item of [
    work,
    personal,
    explicitGpt,
    explicitClaude,
    explicitPersonal,
    restored,
    missing,
    deleted,
    restarted,
    independent,
    resumedPersonal,
    inherited,
    missingInheritedModel,
    overridden,
    mainUnchanged,
    resumedInherited,
    releasedWorktree,
    corruptWorktree,
    branchSession,
    freshMissing,
    corrupt,
  ])
    assert.deepEqual(item.errors, []);
  const version = JSON.parse(
    await readFile(join(packageDir, "package.json"), "utf8"),
  ).version;
  console.log(
    `pi ${version}: strict generated-code types, extension loading/reload, searchable/scrolling picker, two protocols, profile lock, native catalog filtering, selection/cycling, repository restart and /new persistence, switching, refresh, concurrent sessions and unavailable-profile restore passed (${requests.length} synthetic requests).`,
  );
} finally {
  for (const session of sessions) session.dispose();
  server.closeAllConnections();
  await new Promise((done) => server.close(done));
  await rm(root, { recursive: true, force: true });
}
