// Agent connections through the real AgentSetup: each scenario starts from the user's
// own files, runs install/status/remove steps and records each step's result or error
// and the files it changed (path → content and mode, null when deleted). Also the
// credential socket's wire protocol, the generated key helper run against it,
// offeredModels per agent, the setup schema and pi's profile controls source.
import { execFile } from "node:child_process";
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { dirname, join } from "node:path";
import { AppError } from "../../../src/server/errors.ts";
import { agentDefinitions, offeredModels } from "../../../src/server/agents.ts";
import {
  AgentSetup,
  agentSetupSchema,
} from "../../../src/server/agent-setup.ts";
import { enrichModels } from "../../../src/server/model-catalog.ts";
import {
  piProfileControls,
  unprintable,
} from "../../../src/server/pi-profile-controls.ts";
import { agentIds, type Agent } from "../../../src/shared/agents.ts";
import type {
  AgentSetupInput,
  Json,
  Model,
  Profile,
} from "../../../src/shared/types.ts";
import type { JsonObject } from "../../../src/server/json.ts";
import { coreModels } from "./gateway.ts";
import {
  type Case,
  type CaseError,
  caseError,
  modelsDev,
  outcome,
  Placeholders,
  scratch,
  type Recorded,
  syntheticKey,
} from "../lib.ts";

const profile: Profile = {
  id: "11111111-1111-4111-8111-111111111111",
  slug: "work",
  name: "Work: R&D 'quoted'",
  strategy: "fill-first",
  sessionAffinity: true,
  enabled: true,
  createdAt: "2026-01-01T00:00:00.000Z",
};
const other: Profile = {
  ...profile,
  id: "22222222-2222-4222-8222-222222222222",
  slug: "personal",
  name: "Personal",
};
const key = syntheticKey("a");

type Step =
  | {
      op: "install";
      profile?: "other";
      input: Record<string, Json>;
      models: string[];
    }
  | { op: "status"; profile?: "other"; agent: Agent; project?: string }
  | { op: "remove"; profile?: "other"; agent: Agent; project?: string }
  | { op: "write"; path: string; content: string; mode?: number }
  | { op: "delete"; path: string }
  | { op: "copy"; from: string; to: string }
  | { op: "addKey"; path: string; key: string; value: Json }
  | { op: "port"; port: number }
  | { op: "stop" }
  | { op: "start" };
interface Scenario {
  /** Files under the home before the first step, relative to it. */
  files?: Record<string, string>;
  /** Directories under the home created first (mode 0755). */
  directories?: string[];
  /** Agent configuration directories moved from their defaults, relative to the home. */
  moved?: Partial<Record<Agent, string>>;
  steps: Step[];
}

const claudeUser = `${JSON.stringify({ permissions: { deny: ["Bash(rm *)"] }, env: { USER_PREFERENCE: "keep", ANTHROPIC_MODEL: "fixture-model" } }, null, 2)}\n`;
const install = (
  agent: string,
  extra: Record<string, Json> = {},
  models = ["claude-sonnet-4-5", "gpt-5-codex"],
): Step => ({
  op: "install",
  input: {
    agent,
    ...(agent === "claude" && { projectDirectory: "<home>/projects/app" }),
    ...extra,
  },
  models,
});
const settings = "projects/app/.claude/settings.local.json";

const scenarios = {
  "claude fresh project lifecycle": {
    directories: ["projects/app"],
    steps: [
      { op: "status", agent: "claude", project: "<home>/projects/app" },
      install("claude"),
      { op: "status", agent: "claude", project: "<home>/projects/app" },
      { op: "status", agent: "claude" },
      install("claude", {}, ["claude-opus-4-1"]),
      { op: "remove", agent: "claude", project: "<home>/projects/app" },
      { op: "status", agent: "claude", project: "<home>/projects/app" },
    ],
  },
  "claude keeps user settings": {
    directories: ["projects/app"],
    files: { [settings]: claudeUser },
    steps: [
      install("claude", { otherProviders: true }),
      { op: "remove", agent: "claude" },
    ],
  },
  "claude user settings compact json": {
    directories: ["projects/app"],
    files: { [settings]: '{"env":{"A":"1"},"model":"opus"}' },
    steps: [install("claude"), { op: "remove", agent: "claude" }],
  },
  "claude existing api key helper conflicts": {
    directories: ["projects/app"],
    files: {
      [settings]: JSON.stringify({
        apiKeyHelper: "my-existing-helper",
        env: { KEEP: "value" },
      }),
    },
    steps: [install("claude")],
  },
  "claude existing base url conflicts": {
    directories: ["projects/app"],
    files: {
      [settings]: JSON.stringify({
        env: { ANTHROPIC_BASE_URL: "https://elsewhere.example.com" },
      }),
    },
    steps: [install("claude")],
  },
  "claude settings array": {
    directories: ["projects/app"],
    files: { [settings]: "[]" },
    steps: [install("claude")],
  },
  "claude settings malformed": {
    directories: ["projects/app"],
    files: { [settings]: "{" },
    steps: [install("claude")],
  },
  "claude settings empty file": {
    directories: ["projects/app"],
    files: { [settings]: "" },
    steps: [install("claude")],
  },
  "claude settings equal to ours before connecting are retained": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      { op: "copy", from: settings, to: "saved.json" },
      { op: "remove", agent: "claude" },
      { op: "copy", from: "saved.json", to: settings },
      install("claude"),
      { op: "remove", agent: "claude" },
    ],
  },
  "claude user edits our settings": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      { op: "write", path: settings, content: '{"apiKeyHelper":"edited"}\n' },
      { op: "status", agent: "claude" },
      { op: "remove", agent: "claude" },
      install("claude"),
    ],
  },
  "claude user adds their own keys after connecting": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      { op: "addKey", path: settings, key: "model", value: "opus" },
      { op: "status", agent: "claude" },
      install("claude", {}, ["claude-opus-4-1"]),
      { op: "remove", agent: "claude" },
    ],
  },
  "claude launch settings edited": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      {
        op: "write",
        path: "<data>/agent-connections/work/<claude-manifest>.settings.json",
        content: "{}\n",
      },
      { op: "status", agent: "claude" },
      install("claude", {}, ["claude-opus-4-1"]),
      { op: "remove", agent: "claude" },
    ],
  },
  "claude settings file deleted": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      { op: "delete", path: settings },
      { op: "status", agent: "claude" },
      install("claude"),
      { op: "remove", agent: "claude" },
    ],
  },
  "claude two projects": {
    directories: ["projects/app", "projects/api server"],
    steps: [
      install("claude"),
      install("claude", { projectDirectory: "<home>/projects/api server" }, [
        "claude-sonnet-4-5",
      ]),
      { op: "status", agent: "claude" },
      { op: "remove", agent: "claude" },
      { op: "remove", agent: "claude", project: "<home>/projects/api server" },
      { op: "remove", agent: "claude" },
    ],
  },
  "claude project inside a repository": {
    directories: ["repo/.git", "repo/sub"],
    steps: [
      install("claude", { projectDirectory: "<home>/repo/sub" }),
      install("claude", { projectDirectory: "<home>/repo" }),
      { op: "status", agent: "claude", project: "<home>/repo" },
    ],
  },
  "claude project is a worktree": {
    directories: ["main/.git/worktrees/wt", "wt"],
    files: {
      "wt/.git": "gitdir: ../main/.git/worktrees/wt\n",
      "main/.git/worktrees/wt/commondir": "../..\n",
    },
    steps: [install("claude", { projectDirectory: "<home>/wt" })],
  },
  "claude project missing": {
    steps: [install("claude", { projectDirectory: "<home>/nowhere" })],
  },
  "claude project group writable": {
    directories: ["shared"],
    steps: [
      { op: "write", path: "shared/.mode", content: "", mode: 0o775 },
      install("claude", { projectDirectory: "<home>/shared" }),
    ],
  },
  "port change needs reconnect": {
    directories: ["projects/app"],
    steps: [
      install("claude"),
      install("pi"),
      { op: "port", port: 4400 },
      { op: "status", agent: "claude" },
      { op: "status", agent: "pi" },
      install("claude"),
      install("pi"),
      { op: "status", agent: "claude" },
    ],
  },
  "stopped profile": {
    directories: ["projects/app"],
    steps: [
      install("codex"),
      { op: "stop" },
      { op: "status", agent: "codex" },
      install("codex"),
      { op: "remove", agent: "codex" },
      { op: "start" },
      { op: "status", agent: "codex" },
    ],
  },
  "codex fresh home lifecycle": {
    steps: [
      install("codex"),
      { op: "status", agent: "codex" },
      install("codex", { otherProviders: true }, [
        "gpt-5",
        "claude-sonnet-4-5",
      ]),
      { op: "remove", agent: "codex" },
      { op: "status", agent: "codex" },
    ],
  },
  "codex leaves user config untouched": {
    files: {
      ".codex/config.toml":
        'model = "gpt-5-codex"\n\n[projects."/srv/example"]\ntrust_level = "trusted"\n',
    },
    steps: [
      install("codex", { otherProviders: false }),
      { op: "remove", agent: "codex" },
    ],
  },
  "codex our file already exists": {
    files: { ".codex/nonstopvibin-work.config.toml": "user config\n" },
    steps: [install("codex")],
  },
  "codex user edits our file": {
    steps: [
      install("codex"),
      {
        op: "write",
        path: ".codex/nonstopvibin-work.config.toml",
        content: "edited\n",
      },
      { op: "status", agent: "codex" },
      { op: "remove", agent: "codex" },
    ],
  },
  "codex moved directory": {
    moved: { codex: "custom-codex" },
    steps: [install("codex"), { op: "remove", agent: "codex" }],
  },
  "codex two profiles": {
    steps: [
      install("codex"),
      {
        op: "install",
        profile: "other",
        input: { agent: "codex", otherProviders: true },
        models: ["claude-sonnet-4-5", "gpt-5-codex"],
      },
      { op: "status", agent: "codex", profile: "other" },
    ],
  },
  "opencode lifecycle": {
    steps: [
      install("opencode"),
      { op: "status", agent: "opencode" },
      install("opencode", {}, ["gpt-5"]),
      { op: "remove", agent: "opencode" },
    ],
  },
  "opencode plugin exists": {
    files: {
      [`.config/opencode/plugins/nonstopvibin-${profile.id}.js`]: "// mine\n",
    },
    steps: [install("opencode")],
  },
  "opencode moved directory": {
    moved: { opencode: "xdg/opencode" },
    steps: [install("opencode")],
  },
  "pi lifecycle": {
    files: {
      ".pi/agent/models.json": "// Existing commented configuration\n{}",
    },
    steps: [
      install("pi"),
      { op: "status", agent: "pi" },
      { op: "remove", agent: "pi" },
    ],
  },
  "pi extension edited": {
    steps: [
      install("pi"),
      {
        op: "write",
        path: ".pi/agent/extensions/nonstopvibin-work.js",
        content: "// User edit\n",
      },
      { op: "status", agent: "pi" },
      { op: "remove", agent: "pi" },
      install("pi"),
    ],
  },
  "invalid inputs": {
    directories: ["projects/app"],
    steps: [
      install("pi", { projectDirectory: "<home>/projects/app" }),
      { op: "install", input: { agent: "claude" }, models: ["m"] },
      install("claude", { projectDirectory: "relative/path" }),
      install("claude", { projectDirectory: "<home>/projects/app\nx" }),
      install("pi", { otherProviders: true }),
      install("opencode", { otherProviders: false }),
      install("codex", { maxTokens: 40_000 }),
      install("nope"),
      install("codex", {}, []),
      install("codex", {}, ["bad\nmodel"]),
      install("codex", {}, ["a".repeat(201)]),
    ],
  },
} satisfies Record<string, Scenario>;

const home = (root: string) => join(root, "home");
const data = (root: string) => join(root, "data");

function walk(
  root: string,
  base: string,
  skip: string,
): Map<string, { content: string; mode: string }> {
  const files = new Map<string, { content: string; mode: string }>();
  const visit = (directory: string) => {
    if (!existsSync(directory)) return;
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name);
      if (path === skip) continue;
      const status = lstatSync(path);
      if (status.isDirectory()) visit(path);
      else if (status.isFile())
        files.set(path.replace(root, base), {
          content: readFileSync(path, "utf8"),
          mode: `0${(status.mode & 0o777).toString(8)}`,
        });
    }
  };
  visit(root);
  return files;
}

async function runScenario(name: string, scenario: Scenario): Promise<Case> {
  const root = scratch(`agents/${name.replaceAll(" ", "-")}`);
  const homeDirectory = home(root);
  const dataDirectory = data(root);
  mkdirSync(homeDirectory, { mode: 0o755 });
  mkdirSync(dataDirectory, { mode: 0o700 });
  for (const directory of scenario.directories ?? [])
    mkdirSync(join(homeDirectory, directory), { recursive: true, mode: 0o755 });
  for (const [path, content] of Object.entries(scenario.files ?? {})) {
    mkdirSync(dirname(join(homeDirectory, path)), {
      recursive: true,
      mode: 0o755,
    });
    writeFileSync(join(homeDirectory, path), content, { mode: 0o644 });
  }
  let port = 4318;
  let running = true;
  const moved = Object.fromEntries(
    Object.entries(scenario.moved ?? {}).map(([agent, path]) => [
      agent,
      join(homeDirectory, path),
    ]),
  );
  const setup = new AgentSetup(
    dataDirectory,
    (id) => {
      if (!running || (id !== profile.id && id !== other.id))
        throw new AppError(
          "Start this profile before connecting an agent.",
          409,
        );
      return { key: id === profile.id ? key : syntheticKey("b"), port };
    },
    { home: homeDirectory, ...moved },
  );
  const placeholders = new Placeholders()
    .add(setup.socketDirectory, "<socket-dir>")
    .add(dataDirectory, "<data>")
    .add(homeDirectory, "<home>");
  const resolvePath = (path: string) => {
    const claudeManifest = path.includes("<claude-manifest>")
      ? readdirSync(join(setup.directory, "work")).find((n) =>
          /^claude-[0-9a-f]{24}\.json$/.test(n),
        )
      : undefined;
    const expanded = path.replace(
      "<claude-manifest>",
      claudeManifest?.replace(/\.json$/, "") ?? "",
    );
    return expanded.startsWith("<data>")
      ? expanded.replace("<data>", dataDirectory)
      : join(homeDirectory, expanded);
  };
  // The schema inside install() validates what this returns, as the route's does.
  const expand = (value: Record<string, Json>): AgentSetupInput =>
    JSON.parse(JSON.stringify(value).replaceAll("<home>", homeDirectory));
  let before = new Map([
    ...walk(homeDirectory, "<home>", ""),
    ...walk(dataDirectory, "<data>", ""),
  ]);
  const steps: Recorded[] = [];
  try {
    for (const step of scenario.steps) {
      const who =
        step.op !== "port" &&
        step.op !== "stop" &&
        step.op !== "start" &&
        "profile" in step &&
        step.profile === "other"
          ? other
          : profile;
      let result: Recorded;
      let error: CaseError | undefined;
      try {
        switch (step.op) {
          case "install":
            result = await setup.install(who, expand(step.input), step.models);
            break;
          case "status":
            result = await setup.status(
              who,
              step.agent,
              step.project &&
                join(homeDirectory, step.project.replace("<home>/", "")),
            );
            break;
          case "remove":
            await setup.remove(
              who,
              step.agent,
              step.project &&
                join(homeDirectory, step.project.replace("<home>/", "")),
            );
            break;
          case "write":
            if (step.path.endsWith(".mode"))
              chmodSync(dirname(resolvePath(step.path)), step.mode ?? 0o755);
            else writeFileSync(resolvePath(step.path), step.content);
            break;
          case "delete":
            rmSync(resolvePath(step.path));
            break;
          case "addKey": {
            const current: JsonObject = JSON.parse(
              readFileSync(resolvePath(step.path), "utf8"),
            );
            writeFileSync(
              resolvePath(step.path),
              `${JSON.stringify({ ...current, [step.key]: step.value }, null, 2)}\n`,
            );
            break;
          }
          case "copy":
            writeFileSync(
              resolvePath(step.to),
              readFileSync(resolvePath(step.from)),
            );
            break;
          case "port":
            port = step.port;
            break;
          case "stop":
            running = false;
            break;
          case "start":
            running = true;
            break;
        }
      } catch (caught) {
        error = caseError(caught);
      }
      const after = new Map([
        ...walk(homeDirectory, "<home>", ""),
        ...walk(dataDirectory, "<data>", ""),
      ]);
      const changes = new Map<
        string,
        { content: string; mode: string } | null
      >();
      for (const [path, file] of after) {
        const previous = before.get(path);
        if (
          !previous ||
          previous.content !== file.content ||
          previous.mode !== file.mode
        )
          changes.set(path, file);
      }
      for (const path of before.keys())
        if (!after.has(path)) changes.set(path, null);
      before = after;
      steps.push({
        step,
        ...(error ? { error } : { result: result ?? null }),
        changes: Object.fromEntries(changes),
      });
    }
  } finally {
    await setup.close();
    rmSync(setup.socketDirectory, { recursive: true, force: true });
  }
  return placeholders.apply({
    name: `scenario ${name}`,
    input: scenario,
    output: steps,
  });
}

/** Raw requests on the credential socket, and the generated key helper run against it. */
async function credentialSocket(): Promise<Case[]> {
  const root = scratch("agents/credential-socket");
  mkdirSync(home(root), { mode: 0o755 });
  mkdirSync(data(root), { mode: 0o700 });
  let port = 4318;
  let running = true;
  const setup = new AgentSetup(
    data(root),
    (id) => {
      if (!running || id !== profile.id) throw new AppError("stopped", 409);
      return { key, port };
    },
    { home: home(root) },
  );
  const placeholders = new Placeholders()
    .add(setup.socketDirectory, "<socket-dir>")
    .add(data(root), "<data>")
    .add(home(root), "<home>");
  const installed = await setup.install(profile, { agent: "codex" }, ["gpt-5"]);
  const helper =
    installed.files.find((file) => file.includes("codex-key-")) ?? "";
  const raw = (request: string) =>
    new Promise<string>((resolve, reject) => {
      const socket = net.connect(setup.socketPath);
      let response = "";
      socket.on("data", (chunk) => (response += chunk.toString("utf8")));
      socket.on("error", reject);
      socket.on("close", () => resolve(response.replace(/^Date: .*\r\n/m, "")));
      socket.end(request);
    });
  const requests: Array<[string, string]> = [
    [
      "valid",
      `GET /profiles/${profile.id} HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    [
      "host with port",
      `GET /profiles/${profile.id} HTTP/1.1\r\nHost: localhost:80\r\n\r\n`,
    ],
    [
      "other host",
      `GET /profiles/${profile.id} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n`,
    ],
    [
      "origin header",
      `GET /profiles/${profile.id} HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\n\r\n`,
    ],
    [
      "post",
      `POST /profiles/${profile.id} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n`,
    ],
    [
      "upper-case id",
      `GET /profiles/${profile.id.toUpperCase()} HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    [
      "not a uuid",
      `GET /profiles/${"a".repeat(36)} HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    [
      "trailing slash",
      `GET /profiles/${profile.id}/ HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    [
      "query",
      `GET /profiles/${profile.id}?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    [
      "unknown profile",
      `GET /profiles/${other.id} HTTP/1.1\r\nHost: localhost\r\n\r\n`,
    ],
    ["other path", `GET /state HTTP/1.1\r\nHost: localhost\r\n\r\n`],
  ];
  const cases: Case[] = [];
  for (const [name, request] of requests)
    cases.push({
      name: `socket ${name}`,
      input: request,
      output: await raw(request),
    });
  // Asynchronous: the socket is served by this process.
  const runHelper = async (label: string) => {
    const run = await new Promise<{
      status: number;
      stdout: string;
      stderr: string;
    }>((resolve) =>
      execFile(
        "sh",
        [helper],
        { encoding: "utf8", timeout: 10_000 },
        (error, stdout, stderr) =>
          resolve({
            status: error ? Number(error.code ?? 1) : 0,
            stdout,
            stderr,
          }),
      ),
    );
    cases.push({
      name: `key helper ${label}`,
      input: { port, running },
      output: run,
    });
  };
  await runHelper("running");
  port = 4400;
  await runHelper("port changed");
  port = 4318;
  running = false;
  await runHelper("stopped");
  await setup.close();
  rmSync(setup.socketDirectory, { recursive: true, force: true });
  return placeholders.apply(cases);
}

export async function recordAgents(): Promise<Case[]> {
  const cases: Case[] = [];
  for (const [name, scenario] of Object.entries(scenarios))
    cases.push(await runScenario(name, scenario));
  cases.push(...(await credentialSocket()));
  const enrich = async (models: Model[]) =>
    enrichModels(models, [], modelsDev, "2026-11-22T12:00:00.000Z");
  for (const agent of agentIds)
    for (const otherProviders of [false, true])
      cases.push(
        await outcome(
          `offeredModels ${agent} otherProviders ${otherProviders}`,
          { agent, models: coreModels, otherProviders, catalog: "modelsDev" },
          () => offeredModels(agent, coreModels, otherProviders, enrich),
        ),
      );
  const unpriced = coreModels.filter(
    (m) => m.id === "gemini-free" || !m.owned_by,
  );
  for (const agent of ["pi", "opencode"] as const) {
    cases.push(
      await outcome(
        `offeredModels ${agent} nothing priced`,
        { agent, models: unpriced, catalog: "modelsDev" },
        () => offeredModels(agent, unpriced, false, enrich),
      ),
    );
    cases.push(
      await outcome(
        `offeredModels ${agent} no models`,
        { agent, models: [], catalog: "modelsDev" },
        () => offeredModels(agent, [], false, enrich),
      ),
    );
    const offered = await offeredModels(agent, coreModels, false, enrich);
    cases.push({
      name: `entries ${agent}`,
      input: { models: "offeredModels of coreModels" },
      output: agentDefinitions[agent].entries(offered),
    });
  }
  const schemaInputs: Json[] = [
    { agent: "claude", projectDirectory: "/abs/project" },
    { agent: "claude", projectDirectory: "/abs/project", otherProviders: true },
    { agent: "claude" },
    { agent: "claude", projectDirectory: "relative" },
    { agent: "claude", projectDirectory: "/abs/\u0000x" },
    { agent: "claude", projectDirectory: "/abs/‮x" },
    { agent: "codex" },
    { agent: "codex", otherProviders: false },
    { agent: "codex", projectDirectory: "/abs" },
    { agent: "pi", otherProviders: true },
    { agent: "opencode" },
    { agent: "codex", extra: 1 },
    { agent: "Codex" },
    {},
  ];
  for (const [index, value] of schemaInputs.entries())
    cases.push(
      await outcome(`agentSetupSchema ${index}`, value, () =>
        agentSetupSchema.parse(value),
      ),
    );
  cases.push({
    name: "pi profile controls source",
    input: null,
    output: piProfileControls,
  });
  for (const text of [
    "plain",
    "tab\there",
    "line sep",
    "zero​width",
    "bell\u0007",
    "émoji 😀",
  ])
    cases.push({
      name: `unprintable ${JSON.stringify(text)}`,
      input: text,
      output: text.replace(unprintable, ""),
    });
  return cases;
}
