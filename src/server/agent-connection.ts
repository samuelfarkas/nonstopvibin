import type { AgentSetupInput, Profile } from "../shared/types.ts";
import { AppError } from "./errors.ts";

/** Manifest versions releases have written; version 1 has its own schema. */
export const manifestVersions = [2, 3, 4, 5, 6, 7] as const;

/** Starts every provider name a connected agent lists; pi's controls recognise ours by it. */
export const providerPrefix = "nonstopvibin-";
/** The provider a connected agent lists for a profile. */
export const nativeProvider = (profile: Pick<Profile, "slug">) =>
  `${providerPrefix}${profile.slug}`;
/** The gateway's route for one profile; Anthropic clients add `/v1` themselves. */
export const profileBase = (port: number, slug: string) =>
  `http://127.0.0.1:${port}/p/${slug}`;
/** The OpenAI-style base URL of one profile on the gateway. */
export const profileEndpoint = (port: number, slug: string) =>
  `${profileBase(port, slug)}/v1`;
/**
 * How an agent asks `GET /v1/models` for its curated catalog: Codex's own requests
 * carry the header, the generated pi and OpenCode code add the query parameter.
 */
export const catalogRequest = {
  header: "x-nonstopvibin-agent",
  query: "nonstopvibin",
} as const;

export const shellQuote = (value: string) =>
  `'${value.replaceAll("'", "'\\''")}'`;

export interface ConnectionContext {
  profile: Profile;
  input: AgentSetupInput;
  port: number;
  helper: string;
  /** The credential socket, for OpenCode's plugin, which asks it directly. */
  socket: string;
  /** Where pi's extensions remember each repository's profile. */
  piPreferences: string;
}

export interface RecordedConnection {
  version: number;
  content: string;
  models: string[];
}

export interface AgentDefinition {
  /** Manifest version a connection made now records; older ones are migrated. */
  version: (typeof manifestVersions)[number];
  /**
   * Whether a connection from an older version asks the user to reconnect even though
   * startup migrates its files: the agent must reload or launch differently.
   */
  olderNeedsReconnect: boolean;
  /** Where the agent reads its configuration: `$variable`, `$XDG_CONFIG_HOME/xdg`, or `~/home`. */
  directory?: { variable: string; xdg?: string; home: string[] };
  /** Agents connected per project: the project root the configuration belongs to. */
  projectRoot?: (path: string) => Promise<string>;
  /** The configuration file, under the agent's directory or the project, by manifest version. */
  file: (profile: Profile, version: number) => string[];
  /** Whether the connection installs a credential helper that asks the socket. */
  helper: boolean;
  /** The connection's fields or file, as written into the agent's configuration. */
  configure: (context: ConnectionContext) => string;
  /**
   * The configuration with `previous` (our last fields) replaced by `next`; undefined
   * when nothing is left. Only what this app installed is replaced or removed; a
   * collision throws, so existing credentials need no plaintext backup.
   */
  merge: (
    current: string | undefined,
    previous: string | undefined,
    next: string | undefined,
  ) => string | undefined;
  /** Whether the installed configuration still equals what merging our fields gives. */
  unchanged: (current: string, expected: string | undefined) => boolean;
  /** Our settings already in the user's file before connecting, restored on disconnect. */
  retain?: (
    current: string | undefined,
    generated: string,
  ) => string | undefined;
  /** A settings file the launch command passes, derived from the manifest. */
  launchSettings?: (manifest: RecordedConnection) => string | undefined;
  command: (profile: Profile, launchSettings: string) => string;
  instructions: (profile: Profile) => string;
  /** How the agent asks for its curated catalog (see `catalogRequest`). */
  catalog?: "header" | "query";
}

export function conflict(): never {
  throw new AppError(
    "Agent configuration already exists or was edited outside nonstopvibin. Resolve the conflicting settings before reconnecting or disconnecting.",
    409,
  );
}

export const sameText = (current: string, expected: string | undefined) =>
  current === expected;

/** The rule for a file that is wholly ours: any other content is a conflict. */
export function ownedFile(
  current: string | undefined,
  previous: string | undefined,
  next: string | undefined,
): string | undefined {
  if (current !== undefined && current !== previous && current !== next)
    conflict();
  return next;
}
