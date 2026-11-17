import { lstat, realpath } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";
import { z } from "zod";
import type { Profile } from "../shared/types.ts";
import { AppError } from "./errors.ts";
import type { JsonObject } from "./json.ts";
import { isMissing } from "./safe-files.ts";
import {
  type AgentDefinition,
  conflict,
  profileBase,
  shellQuote,
} from "./agent-connection.ts";
import { jsonObject, readConfig } from "./native-config-files.ts";

// Claude Code: our fields in the project's `.claude/settings.local.json`, which the
// user shares with us, plus a `--settings` file carrying the model picker.

const object = z.record(z.string(), z.json());

export function claudeSettings(
  profile: Profile,
  helper: string,
  port: number,
): string {
  return JSON.stringify({
    apiKeyHelper: shellQuote(helper),
    env: {
      CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY: "1",
      CLAUDE_CODE_USE_BEDROCK: "0",
      CLAUDE_CODE_USE_VERTEX: "0",
      CLAUDE_CODE_USE_FOUNDRY: "0",
      CLAUDE_CODE_USE_MANTLE: "0",
      CLAUDE_CODE_USE_ANTHROPIC_AWS: "0",
      ANTHROPIC_CUSTOM_HEADERS: "",
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
      ANTHROPIC_BASE_URL: profileBase(port, profile.slug),
      ANTHROPIC_AUTH_TOKEN: "",
      ANTHROPIC_API_KEY: "",
      CLAUDE_CODE_OAUTH_TOKEN: "",
      CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR: "",
    },
  });
}

// Retain only values equal to our generated, nonsecret settings. Never back up
// arbitrary user configuration: it may contain credentials.
function matchingClaudeConfig(
  current: string | undefined,
  generated: string,
): string | undefined {
  if (current === undefined) return;
  const saved = jsonObject(current);
  const fresh = jsonObject(generated);
  const matching = (existing: JsonObject, fields: JsonObject) =>
    Object.fromEntries(
      Object.entries(fields).filter(
        ([key, value]) =>
          key !== "env" && isDeepStrictEqual(existing[key], value),
      ),
    );
  const env = matching(
    object.parse(saved.env ?? {}),
    object.parse(fresh.env ?? {}),
  );
  return JSON.stringify({
    ...matching(saved, fresh),
    ...(Object.keys(env).length > 0 && { env }),
  });
}

function mergeClaudeConfig(
  current: string | undefined,
  previous: string | undefined,
  next: string | undefined,
): string | undefined {
  const root = current === undefined ? {} : jsonObject(current);
  const oldFields = previous === undefined ? {} : jsonObject(previous);
  const newFields = next === undefined ? {} : jsonObject(next);
  const env = root.env === undefined ? {} : object.parse(root.env);
  const fields: Array<[JsonObject, JsonObject, JsonObject]> = [
    [root, oldFields, newFields],
    [env, object.parse(oldFields.env ?? {}), object.parse(newFields.env ?? {})],
  ];
  for (const [target, old, fresh] of fields) {
    for (const key of new Set([...Object.keys(old), ...Object.keys(fresh)])) {
      if (key === "env") continue;
      if (
        target[key] !== undefined &&
        !isDeepStrictEqual(target[key], old[key]) &&
        !isDeepStrictEqual(target[key], fresh[key])
      )
        conflict();
      if (fresh[key] === undefined) delete target[key];
      else target[key] = fresh[key];
    }
  }
  if (Object.keys(env).length) root.env = env;
  else delete root.env;
  return Object.keys(root).length
    ? JSON.stringify(root, null, 2) + "\n"
    : undefined;
}

export async function claudeProject(path: string): Promise<string> {
  const project = await realpath(path);
  const status = await lstat(project);
  if (
    !status.isDirectory() ||
    status.uid !== process.getuid?.() ||
    (status.mode & 0o022) !== 0
  )
    throw new AppError(
      "Choose a project directory you own, without shared write access.",
    );
  // Claude worktrees inherit the main checkout's local settings. Use that root
  // explicitly rather than promise isolation that the harness does not provide.
  let cursor = project;
  while (cursor !== dirname(cursor)) {
    try {
      const git = await lstat(join(cursor, ".git"));
      if (git.isSymbolicLink() || (!git.isFile() && !git.isDirectory()))
        throw new AppError(
          "The project's Git marker must be an ordinary file or directory without symbolic links.",
        );
      if (git.isFile()) {
        const content = await readConfig(join(cursor, ".git"));
        const gitDir = content?.trim().match(/^gitdir: (.+)$/)?.[1];
        if (gitDir) {
          const common = await readConfig(
            join(resolve(cursor, gitDir), "commondir"),
          );
          if (common)
            throw new AppError(
              "This project is a Git worktree. Claude shares local settings with its main checkout; choose the main checkout instead.",
            );
        }
      }
      break;
    } catch (error) {
      if (!isMissing(error)) throw error;
    }
    cursor = dirname(cursor);
  }
  if (cursor !== project && cursor !== dirname(cursor))
    throw new AppError(
      "Choose the repository root so Claude loads the same settings throughout the project.",
    );
  return project;
}

export const claude: AgentDefinition = {
  version: 3,
  olderNeedsReconnect: true,
  projectRoot: claudeProject,
  file: () => [".claude", "settings.local.json"],
  helper: true,
  configure: ({ profile, helper, port }) =>
    claudeSettings(profile, helper, port),
  merge: mergeClaudeConfig,
  // The file is shared with the user, so key order and formatting are theirs.
  unchanged: (current, expected) =>
    isDeepStrictEqual(jsonObject(current), jsonObject(expected ?? "{}")),
  retain: matchingClaudeConfig,
  launchSettings: (manifest) => {
    // Version 2 connections predate the picker.
    if (manifest.version !== 3) return;
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
  },
  command: (_profile, launchSettings) =>
    `claude --settings ${shellQuote(launchSettings)}`,
  instructions: (profile) =>
    `Run it inside the connected project; plain claude skips the picker. Approve the credential helper, pick a model with /model, and check /status shows ${profile.name}.`,
};
