import http from "node:http";
import { connect } from "node:net";
import { createHash } from "node:crypto";
import { chmod, lstat, mkdir, readdir, rm } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { homedir } from "node:os";
import { isDeepStrictEqual } from "node:util";
import { z } from "zod";
import type {
  Agent,
  AgentSetup as InstalledSetup,
  AgentSetupInput,
  Profile,
} from "../shared/types.ts";
import { AppError } from "./errors.ts";
import {
  credentialHelper,
  nativeConfiguration,
  nativeProvider,
  shellQuote,
} from "./agent-config.ts";
import { openCodePlugin } from "./opencode-plugin.ts";
import {
  configPath,
  isMissing,
  jsonObject,
  matchingClaudeConfig,
  mergeNativeConfig,
  projectDirectory,
  readConfig,
  replaceConfig,
  writeAtomic,
} from "./native-config-files.ts";

const agentSchema = z.enum(["pi", "codex", "opencode", "claude"]);
const projectSchema = z
  .string()
  .min(1)
  .max(4096)
  .refine(
    (value) =>
      isAbsolute(value) &&
      Array.from(value).every(
        (character) =>
          character.charCodeAt(0) >= 32 && character.charCodeAt(0) !== 127,
      ),
    "Choose an absolute project directory.",
  )
  .optional();
export const agentSelectionSchema = z
  .object({ agent: agentSchema, projectDirectory: projectSchema })
  .strict()
  .refine(
    (input) => input.agent === "claude" || input.projectDirectory === undefined,
    "Choose a project for Claude Code only.",
  );
const legacySetupSchema = z
  .object({
    agent: agentSchema,
    model: z
      .string()
      .min(1)
      .max(200)
      .refine(
        (value) =>
          Array.from(value).every(
            (character) =>
              character.charCodeAt(0) >= 32 && character.charCodeAt(0) !== 127,
          ),
        "Model IDs cannot contain control characters.",
      ),
    projectDirectory: projectSchema,
    contextWindow: z.number().int().min(1024).max(2_000_000),
    maxTokens: z.number().int().min(1).max(200_000),
  })
  .strict()
  .refine(
    (input) => input.maxTokens < input.contextWindow,
    "Output budget must be smaller than the context budget.",
  )
  .refine(
    (input) =>
      input.agent === "claude"
        ? Boolean(input.projectDirectory)
        : input.projectDirectory === undefined,
    "Choose a project for Claude Code only.",
  );

export const agentSetupSchema = z
  .object({
    agent: agentSchema,
    projectDirectory: projectSchema,
    otherProviders: z.boolean().optional(),
  })
  .strict()
  .refine(
    (input) =>
      input.agent === "claude"
        ? Boolean(input.projectDirectory)
        : input.projectDirectory === undefined,
    "Choose a project for Claude Code only.",
  )
  .refine(
    (input) =>
      input.agent === "claude" ||
      input.agent === "codex" ||
      input.otherProviders === undefined,
    "Only Claude Code and Codex choose whether to offer other providers.",
  );
const manifestFields = {
  port: z.number().int().min(1).max(65535),
  content: z.string().max(1_000_000),
  retained: z.string().max(1_000_000).optional(),
  helperContent: z.string().max(1_000_000).optional(),
};
const manifestSchema = z.union([
  z
    .object({
      version: z.union([
        z.literal(2),
        z.literal(3),
        z.literal(4),
        z.literal(5),
        z.literal(6),
        z.literal(7),
      ]),
      input: agentSetupSchema,
      models: z.array(z.string().min(1).max(200)).max(10000),
      ...manifestFields,
    })
    .strict(),
  z
    .object({
      version: z.literal(1),
      input: legacySetupSchema,
      ...manifestFields,
    })
    .strict()
    .transform((manifest) => ({
      ...manifest,
      models: [manifest.input.model],
      input: {
        agent: manifest.input.agent,
        projectDirectory: manifest.input.projectDirectory,
      },
    })),
]);
type Manifest = z.infer<typeof manifestSchema>;

interface AgentDirectories {
  home: string;
  codex?: string;
  opencode?: string;
  pi?: string;
}

async function privateDirectory(
  path: string,
  create: boolean,
): Promise<boolean> {
  if (create) await mkdir(path, { recursive: true, mode: 0o700 });
  try {
    const status = await lstat(path);
    if (
      !status.isDirectory() ||
      status.isSymbolicLink() ||
      status.uid !== process.getuid?.() ||
      (status.mode & 0o077) !== 0
    )
      throw new AppError(
        "Agent setup needs an owner-only directory without symbolic links.",
      );
    return true;
  } catch (error) {
    if (!create && isMissing(error)) return false;
    throw error;
  }
}

async function removeStaleSocket(path: string): Promise<void> {
  let status;
  try {
    status = await lstat(path);
  } catch (error) {
    if (isMissing(error)) return;
    throw error;
  }
  if (!status.isSocket() || status.uid !== process.getuid?.())
    throw new AppError(
      "The agent connection path is occupied by an unexpected file.",
    );
  const active = await new Promise<boolean>((resolve, reject) => {
    const client = connect(path);
    client.setTimeout(1000, () => {
      client.destroy();
      reject(new AppError("The existing agent connection did not respond."));
    });
    client.once("connect", () => {
      client.destroy();
      resolve(true);
    });
    client.once("error", (error: NodeJS.ErrnoException) => {
      client.destroy();
      if (error.code === "ECONNREFUSED" || error.code === "ENOENT")
        resolve(false);
      else reject(error);
    });
  });
  if (active)
    throw new AppError(
      "Another nonstopvibin instance is serving these agent connections.",
      409,
    );
  await rm(path, { force: true });
}

export class AgentSetup {
  readonly directory: string;
  readonly socketDirectory: string;
  readonly socketPath: string;
  private server?: http.Server;
  private starting?: Promise<void>;
  private busy = false;
  private directories: AgentDirectories;
  private connection: (profileId: string) => { key: string; port: number };

  constructor(
    directory: string,
    connection: (profileId: string) => { key: string; port: number },
    directories: AgentDirectories = {
      home: homedir(),
      codex: process.env.CODEX_HOME,
      opencode:
        process.env.OPENCODE_CONFIG_DIR ||
        (process.env.XDG_CONFIG_HOME
          ? join(process.env.XDG_CONFIG_HOME, "opencode")
          : undefined),
      pi: process.env.PI_CODING_AGENT_DIR,
    },
  ) {
    this.directory = join(resolve(directory), "agent-connections");
    this.directories = directories;
    // Short path accommodates macOS's 103-byte Unix socket limit, including
    // long home/XDG paths. The directory's owner and mode are checked before use.
    const identity = createHash("sha256")
      .update(resolve(directory))
      .digest("hex")
      .slice(0, 24);
    this.socketDirectory = `/tmp/nonstopvibin-${process.getuid?.()}-${identity}`;
    this.socketPath = join(this.socketDirectory, "agents.sock");
    this.connection = connection;
  }

  async restore(profiles: Profile[] = [], port?: number): Promise<string[]> {
    if (
      (await privateDirectory(this.directory, false)) ||
      (await privateDirectory(
        join(this.directory, "..", "agent-launchers"),
        false,
      ))
    )
      await this.start();
    const failures: string[] = [];
    if (port === undefined) return failures;
    for (const profile of profiles)
      for (const agent of agentSchema.options) {
        let manifests: Manifest[];
        try {
          manifests = await this.manifests(profile, agent);
        } catch {
          failures.push(
            `Could not update ${agent} connection metadata for ${profile.slug}. Open Connect agents to resolve it.`,
          );
          continue;
        }
        for (const manifest of manifests) {
          try {
            const folder = join(this.directory, profile.slug);
            const target = await this.target(
              profile,
              manifest.input,
              false,
              manifest.version,
            );
            const current = await readConfig(target);
            if (current === undefined)
              throw new AppError(
                "The installed agent configuration was removed.",
              );
            const expected = mergeNativeConfig(
              agent,
              current,
              manifest.content,
              manifest.content,
            );
            if (
              agent === "claude" &&
              !isDeepStrictEqual(
                jsonObject(current),
                jsonObject(expected ?? "{}"),
              )
            )
              throw new AppError("Installed connection fields were removed.");
            const helper = join(folder, `${agent}-key-${port}`);
            const content =
              agent === "opencode"
                ? openCodePlugin(profile, this.socketPath, port)
                : nativeConfiguration(profile, manifest.input, helper, port);
            const helperContent =
              agent === "opencode"
                ? undefined
                : credentialHelper(profile, this.socketPath, port);
            const installedHelper =
              agent === "opencode"
                ? undefined
                : await readConfig(this.helper(folder, manifest));
            if (agent !== "opencode" && installedHelper === undefined)
              throw new AppError(
                "The installed credential helper was removed.",
              );
            if (
              installedHelper !== undefined &&
              installedHelper !== helperContent &&
              installedHelper !==
                (manifest.helperContent ??
                  credentialHelper(profile, this.socketPath, manifest.port))
            )
              throw new AppError("The installed credential helper was edited.");
            const pickerContent = this.pickerSettings(manifest);
            const installedPicker =
              pickerContent === undefined
                ? undefined
                : await readConfig(this.pickerPath(folder, manifest.input));
            if (pickerContent !== undefined && installedPicker === undefined)
              throw new AppError("The installed model settings were removed.");
            if (
              manifest.version !== (agent === "pi" ? 7 : 3) ||
              manifest.port !== port ||
              manifest.content !== content ||
              manifest.helperContent !== helperContent ||
              installedHelper !== helperContent ||
              installedPicker !== pickerContent
            )
              await this.install(
                profile,
                manifest.input,
                manifest.models,
                port,
              );
          } catch {
            failures.push(
              `Could not automatically update ${agent} for ${profile.slug}. Open Connect agents to resolve edited or missing files.`,
            );
          }
        }
      }
    return failures;
  }

  private async start(): Promise<void> {
    if (this.server) return;
    if (this.starting) return this.starting;
    this.starting = this.listen();
    try {
      await this.starting;
    } finally {
      this.starting = undefined;
    }
  }

  private async listen(): Promise<void> {
    await privateDirectory(this.socketDirectory, true);
    await removeStaleSocket(this.socketPath);
    const server = http.createServer({ maxHeaderSize: 4096 }, (req, res) => {
      res.setHeader("Cache-Control", "no-store");
      res.setHeader("Content-Type", "text/plain");
      res.setHeader("Connection", "close");
      const match = /^\/profiles\/([a-f0-9-]{36})$/.exec(req.url ?? "");
      if (
        req.method !== "GET" ||
        req.headers.host !== "localhost" ||
        req.headers.origin ||
        !match ||
        !z.string().uuid().safeParse(match[1]).success
      ) {
        res.writeHead(403).end("Agent connection request rejected.");
        return;
      }
      try {
        const { key, port } = this.connection(match[1]);
        if (
          !/^nv_[A-Za-z0-9_-]{43}$/.test(key) ||
          !Number.isInteger(port) ||
          port < 1 ||
          port > 65535
        )
          throw new Error("Invalid agent connection.");
        // Private two-line protocol; never a shell program or management token.
        res.end(`${key}\n${port}`);
      } catch {
        res.writeHead(409).end("Open nonstopvibin and start this profile.");
      }
    });
    server.headersTimeout = 5000;
    server.requestTimeout = 5000;
    server.setTimeout(5000, (socket) => socket.destroy());
    try {
      await new Promise<void>((resolve, reject) => {
        server.once("error", reject);
        server.listen(this.socketPath, resolve);
      });
      await chmod(this.socketPath, 0o600);
      this.server = server;
    } catch (error) {
      server.close();
      throw error;
    }
  }

  private async folder(
    profile: Profile,
    create: boolean,
  ): Promise<string | undefined> {
    z.string()
      // Older profile names could leave trailing or doubled separators after truncation.
      .regex(/^[a-z0-9][a-z0-9-]*$/)
      .parse(profile.slug);
    if (!(await privateDirectory(this.directory, create))) return;
    const path = join(this.directory, profile.slug);
    return (await privateDirectory(path, create)) ? path : undefined;
  }

  private manifestName(agent: Agent, project?: string): string {
    return agent === "claude" && project
      ? `claude-${createHash("sha256").update(project).digest("hex").slice(0, 24)}.json`
      : `${agent}.json`;
  }

  private async manifests(profile: Profile, agent: Agent): Promise<Manifest[]> {
    const folder = await this.folder(profile, false);
    if (!folder) return [];
    const names =
      agent === "claude"
        ? (await readdir(folder)).filter((name) =>
            /^claude-[a-f0-9]{24}\.json$/.test(name),
          )
        : [`${agent}.json`];
    const manifests: Manifest[] = [];
    for (const name of names) {
      const content = await readConfig(join(folder, name));
      if (content === undefined) continue;
      const manifest = manifestSchema.parse(jsonObject(content));
      if (
        manifest.input.agent !== agent ||
        this.manifestName(agent, manifest.input.projectDirectory) !== name
      )
        throw new AppError("Agent setup metadata is invalid.");
      manifests.push(manifest);
    }
    return manifests.sort((a, b) =>
      (a.input.projectDirectory ?? "").localeCompare(
        b.input.projectDirectory ?? "",
      ),
    );
  }

  private async manifest(
    profile: Profile,
    agent: Agent,
    project?: string,
  ): Promise<Manifest | undefined> {
    const manifests = await this.manifests(profile, agent);
    return project === undefined
      ? manifests[0]
      : manifests.find(
          (manifest) => manifest.input.projectDirectory === project,
        );
  }

  private helper(folder: string, manifest: Manifest): string {
    return join(folder, `${manifest.input.agent}-key-${manifest.port}`);
  }

  private pickerPath(folder: string, input: AgentSetupInput): string {
    return join(
      folder,
      this.manifestName("claude", input.projectDirectory).replace(
        /\.json$/,
        ".settings.json",
      ),
    );
  }

  private pickerSettings(manifest: Manifest | undefined): string | undefined {
    if (
      !manifest ||
      manifest.version !== 3 ||
      manifest.input.agent !== "claude"
    )
      return;
    // Project/local settings ignore modelPicker. --settings carries the same
    // immutable profile route plus the real model IDs, without choosing a model.
    return (
      JSON.stringify(
        {
          ...jsonObject(manifest.content),
          modelPicker: {
            options: manifest.models.map((model) => ({ model })),
            replaceBuiltInOptions: true,
          },
        },
        null,
        2,
      ) + "\n"
    );
  }

  private async target(
    profile: Profile,
    input: AgentSetupInput,
    create: boolean,
    version = 7,
  ): Promise<string> {
    const { home, codex, opencode, pi } = this.directories;
    if (input.agent === "claude") {
      if (!input.projectDirectory)
        throw new AppError("Choose a project for Claude Code.");
      try {
        return await configPath(
          await projectDirectory(input.projectDirectory),
          [".claude", "settings.local.json"],
          create,
        );
      } catch (error) {
        if (!create && isMissing(error))
          return join(input.projectDirectory, ".claude", "settings.local.json");
        throw error;
      }
    }
    const custom =
      input.agent === "codex"
        ? codex
        : input.agent === "opencode"
          ? opencode
          : pi;
    if (create) await mkdir(custom || home, { recursive: true, mode: 0o700 });
    const names =
      input.agent === "codex"
        ? [
            ...(custom ? [] : [".codex"]),
            `nonstopvibin-${profile.slug}.config.toml`,
          ]
        : input.agent === "opencode"
          ? [
              ...(custom ? [] : [".config", "opencode"]),
              "plugins",
              `nonstopvibin-${profile.id}.js`,
            ]
          : [
              ...(custom ? [] : [".pi", "agent"]),
              "extensions",
              `nonstopvibin-${version < 6 ? profile.id : profile.slug}.js`,
            ];
    return configPath(custom || home, names, create);
  }

  private async result(
    profile: Profile,
    manifest: Manifest,
  ): Promise<InstalledSetup> {
    const { input } = manifest;
    const folder = join(this.directory, profile.slug);
    const files = [
      await this.target(profile, input, false, manifest.version),
      join(folder, this.manifestName(input.agent, input.projectDirectory)),
    ];
    if (input.agent !== "opencode") files.push(this.helper(folder, manifest));
    if (this.pickerSettings(manifest))
      files.push(this.pickerPath(folder, input));
    return {
      ...input,
      models: manifest.models,
      ...(input.agent === "claude" && {
        projects: (await this.manifests(profile, "claude"))
          .map((manifest) => manifest.input.projectDirectory!)
          .sort(),
      }),
      provider: nativeProvider(profile),
      command:
        input.agent === "codex"
          ? `codex --profile nonstopvibin-${profile.slug}`
          : input.agent === "claude"
            ? `claude --settings ${shellQuote(this.pickerPath(folder, input))}`
            : input.agent,
      files,
      instructions:
        input.agent === "claude"
          ? `Run it inside the connected project; plain claude skips the picker. Approve the credential helper, pick a model with /model, and check /status shows ${profile.name}.`
          : input.agent === "codex"
            ? `Works in any project. Pick a model with /model before the first prompt; Codex's built-in default may not exist here. Needs Codex 0.140.0 or later.`
            : input.agent === "pi"
              ? `Choose ${profile.name} once with /nv; pi remembers the profile and model for this repository, including /new. Use native /model to switch models. The footer shows the active profile. App startup updates connected extensions automatically; restart pi or /reload to load changes. Skip /login: a stored key would override the credential helper. Needs pi 0.85.1 or later.`
              : `Use /models to choose nonstopvibin · ${profile.name}. Needs OpenCode 1.18.29 or later; background model overrides to another provider are rejected.`,
    };
  }

  async status(
    profile: Profile,
    agent: Agent,
    project?: string,
  ): Promise<InstalledSetup | null> {
    const manifest = await this.manifest(profile, agent, project);
    if (!manifest) return null;
    const result = await this.result(profile, manifest);
    if (
      manifest.version === 1 ||
      (agent === "pi" && manifest.version !== 7) ||
      (agent === "claude" && manifest.version !== 3)
    )
      result.needsReconnect = true;
    if (
      agent === "claude" &&
      manifest.version === 3 &&
      (await readConfig(
        this.pickerPath(join(this.directory, profile.slug), manifest.input),
      )) !== this.pickerSettings(manifest)
    )
      result.needsReconnect = true;
    const current = await readConfig(result.files[0]);
    const expected = mergeNativeConfig(
      agent,
      current,
      manifest.content,
      manifest.content,
    );
    if (
      current === undefined ||
      (agent !== "claude"
        ? current !== expected
        : !isDeepStrictEqual(jsonObject(current), jsonObject(expected ?? "{}")))
    )
      result.needsReconnect = true;
    if (
      agent !== "opencode" &&
      (await readConfig(
        this.helper(join(this.directory, profile.slug), manifest),
      )) !== credentialHelper(profile, this.socketPath, manifest.port)
    )
      result.needsReconnect = true;
    if (this.connection(profile.id).port !== manifest.port)
      result.needsReconnect = true;
    return result;
  }

  async install(
    profile: Profile,
    input: AgentSetupInput,
    models: string[],
    port?: number,
  ): Promise<InstalledSetup> {
    const validated = agentSetupSchema.parse(input);
    z.string().uuid().parse(profile.id);
    const catalog = z
      .array(
        z
          .string()
          .min(1)
          .max(200)
          .refine(
            (value) =>
              Array.from(value).every(
                (character) =>
                  character.charCodeAt(0) >= 32 &&
                  character.charCodeAt(0) !== 127,
              ),
            "Model IDs cannot contain control characters.",
          ),
      )
      .min(1)
      .max(10000)
      .parse(models);
    // ponytail: serialize shared config edits; per-target locks if contention matters.
    if (this.busy)
      throw new AppError("Agent setup is already changing. Try again.", 409);
    this.busy = true;
    try {
      if (validated.projectDirectory)
        validated.projectDirectory = await projectDirectory(
          validated.projectDirectory,
        );
      const previous = await this.manifest(
        profile,
        validated.agent,
        validated.projectDirectory,
      );
      port ??= this.connection(profile.id).port;
      z.number().int().min(1).max(65535).parse(port);
      const folder = await this.folder(profile, true);
      if (!folder)
        throw new AppError("Could not create the agent connection directory.");
      const manifest: Manifest = {
        version: validated.agent === "pi" ? 7 : 3,
        input: validated,
        models: catalog,
        port,
        content: "",
      };
      const helper = this.helper(folder, manifest);
      if (validated.agent !== "opencode")
        manifest.helperContent = credentialHelper(
          profile,
          this.socketPath,
          port,
        );
      manifest.content =
        validated.agent === "opencode"
          ? openCodePlugin(profile, this.socketPath, port)
          : nativeConfiguration(profile, validated, helper, port);
      const target = await this.target(profile, validated, true);
      const before = await readConfig(target);
      const previousTarget = previous
        ? await this.target(profile, previous.input, false, previous.version)
        : target;
      const previousContent =
        previousTarget === target
          ? undefined
          : await readConfig(previousTarget);
      if (
        previousContent !== undefined &&
        previousContent !== previous?.content
      )
        throw new AppError(
          "The old pi extension was edited outside nonstopvibin. Resolve it before reconnecting.",
          409,
        );
      const after = mergeNativeConfig(
        validated.agent,
        before,
        previous?.content,
        manifest.content,
      );
      if (validated.agent === "claude")
        manifest.retained = previous
          ? previous.retained
          : matchingClaudeConfig(before, manifest.content);
      const pickerPath = this.pickerPath(folder, validated);
      const pickerAfter = this.pickerSettings(manifest);
      const pickerBefore =
        pickerAfter === undefined ? undefined : await readConfig(pickerPath);
      if (
        pickerBefore !== undefined &&
        pickerBefore !== this.pickerSettings(previous) &&
        pickerBefore !== pickerAfter
      )
        throw new AppError(
          "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before syncing.",
          409,
        );
      const manifestContent = JSON.stringify(manifest, null, 2) + "\n";
      if (
        Buffer.byteLength(manifestContent) > 1_000_000 ||
        Buffer.byteLength(pickerAfter ?? "") > 1_000_000
      )
        throw new AppError(
          "The model catalog is too large for native agent settings.",
        );
      await this.start();
      if (manifest.helperContent !== undefined) {
        await readConfig(helper); // Refuse symlinks and non-owned files before replacement.
        await writeAtomic(helper, manifest.helperContent, 0o700);
      }
      await replaceConfig(target, before, after);
      let previousRemoved = false;
      try {
        if (previousContent !== undefined) {
          await replaceConfig(previousTarget, previousContent, undefined);
          previousRemoved = true;
        }
        if (pickerAfter !== undefined)
          await replaceConfig(pickerPath, pickerBefore, pickerAfter);
        try {
          await writeAtomic(
            join(
              folder,
              this.manifestName(validated.agent, validated.projectDirectory),
            ),
            manifestContent,
          );
        } catch (error) {
          if (pickerAfter !== undefined)
            await replaceConfig(pickerPath, pickerAfter, pickerBefore);
          throw error;
        }
      } catch (error) {
        if (previousRemoved)
          await replaceConfig(previousTarget, undefined, previousContent);
        await replaceConfig(target, after, before);
        throw error;
      }
      return this.result(profile, manifest);
    } finally {
      this.busy = false;
    }
  }

  async remove(
    profile: Profile,
    agent: Agent,
    project?: string,
  ): Promise<void> {
    if (this.busy)
      throw new AppError("Agent setup is already changing. Try again.", 409);
    this.busy = true;
    try {
      if (
        agent === "claude" &&
        project === undefined &&
        (await this.manifests(profile, agent)).length > 1
      )
        throw new AppError("Choose which Claude project to disconnect.");
      const manifest = await this.manifest(profile, agent, project);
      if (!manifest) return;
      const target = await this.target(
        profile,
        manifest.input,
        false,
        manifest.version,
      );
      const before = await readConfig(target);
      const folder = join(this.directory, profile.slug);
      const pickerPath = this.pickerPath(folder, manifest.input);
      const pickerBefore =
        this.pickerSettings(manifest) === undefined
          ? undefined
          : await readConfig(pickerPath);
      if (
        pickerBefore !== undefined &&
        pickerBefore !== this.pickerSettings(manifest)
      )
        throw new AppError(
          "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before disconnecting.",
          409,
        );
      const after = mergeNativeConfig(
        agent,
        before,
        manifest.content,
        before === undefined ? undefined : manifest.retained,
      );
      await replaceConfig(target, before, after);
      if (pickerBefore !== undefined) {
        try {
          await replaceConfig(pickerPath, pickerBefore, undefined);
        } catch (error) {
          await replaceConfig(target, after, before);
          throw error;
        }
      }
      await rm(
        join(folder, this.manifestName(agent, manifest.input.projectDirectory)),
        { force: true },
      );
      if (
        agent !== "opencode" &&
        !(await this.manifests(profile, agent)).length
      ) {
        for (const name of await readdir(folder))
          if (new RegExp(`^${agent}-key-[0-9]{1,5}$`).test(name))
            await rm(join(folder, name), { force: true });
      }
    } finally {
      this.busy = false;
    }
  }

  async close(): Promise<void> {
    if (this.starting) await this.starting;
    const server = this.server;
    if (!server) return;
    this.server = undefined;
    await new Promise<void>((resolve) => {
      server.close(() => resolve());
      server.closeAllConnections();
    });
  }
}
