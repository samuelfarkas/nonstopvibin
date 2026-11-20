import type http from "node:http";
import { createHash } from "node:crypto";
import { lstat, mkdir, readdir, rm } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { homedir } from "node:os";
import { z } from "zod";
import type {
  AgentSetup as InstalledSetup,
  AgentSetupInput,
  Profile,
} from "../shared/types.ts";
import { agentIds, agents, type Agent } from "../shared/agents.ts";
import { AppError } from "./errors.ts";
import {
  agentDefinitions,
  configDirectory,
  defaultConfigDirectory,
} from "./agents.ts";
import {
  type ConnectionContext,
  manifestVersions,
  nativeProvider,
} from "./agent-connection.ts";
import { agentConnectionsDirectory } from "./data-directory.ts";
import { credentialHelper, serveCredentials } from "./credential-socket.ts";
import {
  configPath,
  jsonObject,
  readConfig,
  replaceConfig,
} from "./native-config-files.ts";
import { isMissing, ownEntry, writeAtomic } from "./safe-files.ts";

const agentSchema = z.enum(agentIds);
const withoutControls = (value: string) =>
  Array.from(value).every(
    (character) =>
      character.charCodeAt(0) >= 32 && character.charCodeAt(0) !== 127,
  );
const modelId = z
  .string()
  .min(1)
  .max(200)
  .refine(withoutControls, "Model IDs cannot contain control characters.");
/**
 * Claude Code connects one project at a time; the other agents take none. Connecting
 * `requires` the project; looking a connection up may leave it out.
 */
const takesProject =
  (requires: boolean) => (input: { agent: Agent; projectDirectory?: string }) =>
    agents[input.agent].project
      ? !requires || Boolean(input.projectDirectory)
      : input.projectDirectory === undefined;
const projectMessage = "Choose a project for Claude Code only.";
const projectSchema = z
  .string()
  .min(1)
  .max(4096)
  .refine(
    (value) => isAbsolute(value) && withoutControls(value),
    "Choose an absolute project directory.",
  )
  .optional();
export const agentSelectionSchema = z
  .object({ agent: agentSchema, projectDirectory: projectSchema })
  .strict()
  .refine(takesProject(false), projectMessage);
const legacySetupSchema = z
  .object({
    agent: agentSchema,
    model: modelId,
    projectDirectory: projectSchema,
    contextWindow: z.number().int().min(1024).max(2_000_000),
    maxTokens: z.number().int().min(1).max(200_000),
  })
  .strict()
  .refine(
    (input) => input.maxTokens < input.contextWindow,
    "Output budget must be smaller than the context budget.",
  )
  .refine(takesProject(true), projectMessage);

export const agentSetupSchema = z
  .object({
    agent: agentSchema,
    projectDirectory: projectSchema,
    otherProviders: z.boolean().optional(),
  })
  .strict()
  .refine(takesProject(true), projectMessage)
  .refine(
    (input) => agents[input.agent].native || input.otherProviders === undefined,
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
      version: z.union(manifestVersions.map((version) => z.literal(version))),
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

/** The home directory, and each agent's configuration directory when moved from it. */
type AgentDirectories = { home: string } & Partial<Record<Agent, string>>;

// Layout of the agent connections directory: `<slug>/<agent>.json` manifests (one per
// project for Claude, with its `.settings.json`), `<slug>/<agent>-key-<port>` helpers
// and pi's `pi-preferences`. A port change installs a new helper and leaves the old
// one failing closed for agents still running with it.
const helperPrefix = (agent: Agent) => `${agent}-key-`;

async function privateDirectory(
  path: string,
  create: boolean,
): Promise<boolean> {
  if (create) await mkdir(path, { recursive: true, mode: 0o700 });
  try {
    if (!ownEntry(await lstat(path), "directory", 0o077))
      throw new AppError(
        "Agent setup needs an owner-only directory without symbolic links.",
      );
    return true;
  } catch (error) {
    if (!create && isMissing(error)) return false;
    throw error;
  }
}

export class AgentSetup {
  readonly directory: string;
  readonly socketDirectory: string;
  readonly socketPath: string;
  private readonly piPreferences: string;
  private server?: http.Server;
  private starting?: Promise<void>;
  private busy = false;
  private directories: AgentDirectories;
  private connection: (profileId: string) => { key: string; port: number };

  /**
   * `connection` answers the credential socket and the port agents use now; it throws
   * while the profile is not running, and the app's error passes through to the routes.
   */
  constructor(
    directory: string,
    connection: (profileId: string) => { key: string; port: number },
    directories: AgentDirectories = {
      home: homedir(),
      ...Object.fromEntries(
        agentIds.map((agent) => [agent, configDirectory(agent)]),
      ),
    },
  ) {
    this.directory = agentConnectionsDirectory(resolve(directory));
    this.piPreferences = join(this.directory, "pi-preferences");
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

  /**
   * At startup, rewrites connections whose files are still exactly ours but stale
   * (another port, profile name, template or manifest version). Anything removed or
   * edited is left alone and reported for the user to resolve in Connect agents.
   */
  async restore(profiles: Profile[], port: number): Promise<string[]> {
    if (await privateDirectory(this.directory, false)) await this.start();
    const failures: string[] = [];
    for (const profile of profiles)
      for (const agent of agentIds) {
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
            const definition = agentDefinitions[agent];
            const folder = join(this.directory, profile.slug);
            const installed = await this.installed(profile, manifest);
            if (!installed.configIntact)
              throw new AppError("Installed connection fields were removed.");
            const content = definition.configure(
              this.context(profile, manifest.input, folder, port),
            );
            const helperContent = definition.helper
              ? credentialHelper(profile, this.socketPath, port)
              : undefined;
            if (definition.helper && installed.helper === undefined)
              throw new AppError(
                "The installed credential helper was removed.",
              );
            // A helper from an earlier template or port is still ours to replace.
            if (
              installed.helper !== undefined &&
              installed.helper !== helperContent &&
              installed.helper !==
                (manifest.helperContent ??
                  credentialHelper(profile, this.socketPath, manifest.port))
            )
              throw new AppError("The installed credential helper was edited.");
            if (
              installed.launchSettings !== undefined &&
              installed.launchFile === undefined
            )
              throw new AppError("The installed model settings were removed.");
            if (
              manifest.version !== definition.version ||
              manifest.port !== port ||
              manifest.content !== content ||
              manifest.helperContent !== helperContent ||
              installed.helper !== helperContent ||
              installed.launchFile !== installed.launchSettings
            )
              await this.write(
                profile,
                manifest.input,
                manifest.models,
                () => port,
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

  /**
   * A connection's installed files, read in the order status reports problems in.
   * `configIntact`: the agent's configuration still holds every field the connection
   * wrote; reading it throws when the user's settings collide with ours.
   */
  private async installed(profile: Profile, manifest: Manifest) {
    const { input } = manifest;
    const definition = agentDefinitions[input.agent];
    const folder = join(this.directory, profile.slug);
    const launchSettings = this.launchSettings(manifest);
    const launchFile =
      launchSettings === undefined
        ? undefined
        : await readConfig(this.launchSettingsPath(folder, input));
    const config = await readConfig(
      await this.target(profile, input, false, manifest.version),
    );
    const expected = definition.merge(
      config,
      manifest.content,
      manifest.content,
    );
    const helper = definition.helper
      ? await readConfig(this.helperPath(folder, input.agent, manifest.port))
      : undefined;
    return {
      launchSettings,
      launchFile,
      helper,
      configIntact:
        config !== undefined && definition.unchanged(config, expected),
    };
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
    this.server = await serveCredentials(this.socketPath, this.connection);
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
    return agents[agent].project && project
      ? `${agent}-${createHash("sha256").update(project).digest("hex").slice(0, 24)}.json`
      : `${agent}.json`;
  }

  private async manifests(profile: Profile, agent: Agent): Promise<Manifest[]> {
    const folder = await this.folder(profile, false);
    if (!folder) return [];
    const names = agents[agent].project
      ? (await readdir(folder)).filter((name) =>
          new RegExp(`^${agent}-[a-f0-9]{24}\\.json$`).test(name),
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

  private helperPath(folder: string, agent: Agent, port: number): string {
    return join(folder, `${helperPrefix(agent)}${port}`);
  }

  private launchSettingsPath(folder: string, input: AgentSetupInput): string {
    return join(
      folder,
      this.manifestName(input.agent, input.projectDirectory).replace(
        /\.json$/,
        ".settings.json",
      ),
    );
  }

  private launchSettings(manifest: Manifest | undefined): string | undefined {
    return (
      manifest &&
      agentDefinitions[manifest.input.agent].launchSettings?.(manifest)
    );
  }

  private context(
    profile: Profile,
    input: AgentSetupInput,
    folder: string,
    port: number,
  ): ConnectionContext {
    return {
      profile,
      input,
      port,
      helper: this.helperPath(folder, input.agent, port),
      socket: this.socketPath,
      piPreferences: this.piPreferences,
    };
  }

  private async target(
    profile: Profile,
    input: AgentSetupInput,
    create: boolean,
    version: number,
  ): Promise<string> {
    const definition = agentDefinitions[input.agent];
    const names = definition.file(profile, version);
    if (definition.projectRoot) {
      if (!input.projectDirectory)
        throw new AppError(
          `Choose a project for ${agents[input.agent].title}.`,
        );
      try {
        return await configPath(
          await definition.projectRoot(input.projectDirectory),
          names,
          create,
        );
      } catch (error) {
        if (!create && isMissing(error))
          return join(input.projectDirectory, ...names);
        throw error;
      }
    }
    const { home } = this.directories;
    const custom = this.directories[input.agent];
    if (create) await mkdir(custom || home, { recursive: true, mode: 0o700 });
    const base = custom ? [] : defaultConfigDirectory(input.agent);
    return configPath(custom || home, [...base, ...names], create);
  }

  private async result(
    profile: Profile,
    manifest: Manifest,
  ): Promise<InstalledSetup> {
    const { input } = manifest;
    const definition = agentDefinitions[input.agent];
    const folder = join(this.directory, profile.slug);
    const launchSettings = this.launchSettingsPath(folder, input);
    const files = [
      await this.target(profile, input, false, manifest.version),
      join(folder, this.manifestName(input.agent, input.projectDirectory)),
    ];
    if (definition.helper)
      files.push(this.helperPath(folder, input.agent, manifest.port));
    if (this.launchSettings(manifest)) files.push(launchSettings);
    return {
      ...input,
      models: manifest.models,
      ...(agents[input.agent].project && {
        projects: (await this.manifests(profile, input.agent))
          .map((manifest) => manifest.input.projectDirectory!)
          .sort(),
      }),
      provider: nativeProvider(profile),
      command: definition.command(profile, launchSettings),
      files,
      instructions: definition.instructions(profile),
    };
  }

  /**
   * The connection, with `needsReconnect` when the agent would fail or keep stale
   * settings as installed: the user must act. Startup's `restore` repairs only files
   * that are still exactly ours; the rest it reports.
   */
  async status(
    profile: Profile,
    agent: Agent,
    project?: string,
  ): Promise<InstalledSetup | null> {
    const manifest = await this.manifest(profile, agent, project);
    if (!manifest) return null;
    const definition = agentDefinitions[agent];
    const result = await this.result(profile, manifest);
    const installed = await this.installed(profile, manifest);
    // Like install, status answers only for a running profile: `connection` refuses a
    // stopped one with a 409, which the route returns (the app asks only while running).
    const port = this.connection(profile.id).port;
    if (
      manifest.version === 1 ||
      (definition.olderNeedsReconnect &&
        manifest.version !== definition.version) ||
      installed.launchFile !== installed.launchSettings ||
      !installed.configIntact ||
      (definition.helper &&
        installed.helper !==
          credentialHelper(profile, this.socketPath, manifest.port)) ||
      port !== manifest.port
    )
      result.needsReconnect = true;
    return result;
  }

  /** Connects an agent from the app; `connection` refuses a stopped profile. */
  async install(
    profile: Profile,
    input: AgentSetupInput,
    models: string[],
  ): Promise<InstalledSetup> {
    return this.write(
      profile,
      input,
      models,
      () => this.connection(profile.id).port,
    );
  }

  /** `gatewayPort` is asked once the inputs and the existing files are checked. */
  private async write(
    profile: Profile,
    input: AgentSetupInput,
    models: string[],
    gatewayPort: () => number,
  ): Promise<InstalledSetup> {
    const validated = agentSetupSchema.parse(input);
    z.string().uuid().parse(profile.id);
    const catalog = z.array(modelId).min(1).max(10000).parse(models);
    const definition = agentDefinitions[validated.agent];
    // ponytail: serialize shared config edits; per-target locks if contention matters.
    if (this.busy)
      throw new AppError("Agent setup is already changing. Try again.", 409);
    this.busy = true;
    try {
      if (validated.projectDirectory && definition.projectRoot)
        validated.projectDirectory = await definition.projectRoot(
          validated.projectDirectory,
        );
      const previous = await this.manifest(
        profile,
        validated.agent,
        validated.projectDirectory,
      );
      const port = gatewayPort();
      z.number().int().min(1).max(65535).parse(port);
      const folder = await this.folder(profile, true);
      if (!folder)
        throw new AppError("Could not create the agent connection directory.");
      const manifest: Manifest = {
        version: definition.version,
        input: validated,
        models: catalog,
        port,
        content: "",
      };
      const helper = this.helperPath(folder, validated.agent, port);
      if (definition.helper)
        manifest.helperContent = credentialHelper(
          profile,
          this.socketPath,
          port,
        );
      manifest.content = definition.configure(
        this.context(profile, validated, folder, port),
      );
      const target = await this.target(
        profile,
        validated,
        true,
        definition.version,
      );
      const before = await readConfig(target);
      const previousTarget = previous
        ? await this.target(profile, previous.input, false, previous.version)
        : target;
      const previousContent =
        previousTarget === target
          ? undefined
          : await readConfig(previousTarget);
      // Only pi's file moved between manifest versions (see its definition).
      if (
        previousContent !== undefined &&
        previousContent !== previous?.content
      )
        throw new AppError(
          "The old pi extension was edited outside nonstopvibin. Resolve it before reconnecting.",
          409,
        );
      const after = definition.merge(
        before,
        previous?.content,
        manifest.content,
      );
      if (definition.retain)
        manifest.retained = previous
          ? previous.retained
          : definition.retain(before, manifest.content);
      const launchPath = this.launchSettingsPath(folder, validated);
      const launchAfter = this.launchSettings(manifest);
      const launchBefore =
        launchAfter === undefined ? undefined : await readConfig(launchPath);
      if (
        launchBefore !== undefined &&
        launchBefore !== this.launchSettings(previous) &&
        launchBefore !== launchAfter
      )
        throw new AppError(
          "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before syncing.",
          409,
        );
      const manifestContent = JSON.stringify(manifest, null, 2) + "\n";
      if (
        Buffer.byteLength(manifestContent) > 1_000_000 ||
        Buffer.byteLength(launchAfter ?? "") > 1_000_000
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
        if (launchAfter !== undefined)
          await replaceConfig(launchPath, launchBefore, launchAfter);
        try {
          await writeAtomic(
            join(
              folder,
              this.manifestName(validated.agent, validated.projectDirectory),
            ),
            manifestContent,
          );
        } catch (error) {
          if (launchAfter !== undefined)
            await replaceConfig(launchPath, launchAfter, launchBefore);
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
      const definition = agentDefinitions[agent];
      if (
        agents[agent].project &&
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
      const launchPath = this.launchSettingsPath(folder, manifest.input);
      const launchBefore =
        this.launchSettings(manifest) === undefined
          ? undefined
          : await readConfig(launchPath);
      if (
        launchBefore !== undefined &&
        launchBefore !== this.launchSettings(manifest)
      )
        throw new AppError(
          "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before disconnecting.",
          409,
        );
      const after = definition.merge(
        before,
        manifest.content,
        before === undefined ? undefined : manifest.retained,
      );
      await replaceConfig(target, before, after);
      if (launchBefore !== undefined) {
        try {
          await replaceConfig(launchPath, launchBefore, undefined);
        } catch (error) {
          await replaceConfig(target, after, before);
          throw error;
        }
      }
      await rm(
        join(folder, this.manifestName(agent, manifest.input.projectDirectory)),
        { force: true },
      );
      if (definition.helper && !(await this.manifests(profile, agent)).length) {
        const prefix = helperPrefix(agent);
        for (const name of await readdir(folder))
          if (
            name.startsWith(prefix) &&
            /^[0-9]{1,5}$/.test(name.slice(prefix.length))
          )
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
