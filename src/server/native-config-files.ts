import { constants } from "node:fs";
import {
  lstat,
  mkdir,
  open,
  realpath,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { isDeepStrictEqual } from "node:util";
import { z } from "zod";
import { AppError } from "./errors.ts";
import type { Agent } from "../shared/agents.ts";
import type { JsonObject } from "./json.ts";

export function isMissing(cause: unknown): boolean {
  return cause instanceof Error && "code" in cause && cause.code === "ENOENT";
}

// The user selects the root; generated descendants must never traverse symlinks.
export async function configPath(
  root: string,
  names: string[],
  create: boolean,
): Promise<string> {
  const canonical = await realpath(root);
  const validateDirectory = async (folder: string) => {
    const status = await lstat(folder);
    if (
      !status.isDirectory() ||
      status.isSymbolicLink() ||
      status.uid !== process.getuid?.() ||
      (status.mode & 0o022) !== 0
    )
      throw new AppError(
        "Agent configuration needs a directory you own, without symbolic links or shared write access.",
      );
  };
  await validateDirectory(canonical);
  let folder = canonical;
  for (const name of names.slice(0, -1)) {
    folder = join(folder, name);
    if (create)
      await mkdir(folder, { mode: 0o700 }).catch(
        (error: NodeJS.ErrnoException) => {
          if (error.code !== "EEXIST") throw error;
        },
      );
    try {
      await validateDirectory(folder);
    } catch (error) {
      if (!create && isMissing(error)) return join(canonical, ...names);
      throw error;
    }
  }
  return join(folder, names.at(-1)!);
}

export async function readConfig(path: string): Promise<string | undefined> {
  try {
    const file = await open(
      path,
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
    try {
      const status = await file.stat();
      if (
        !status.isFile() ||
        status.uid !== process.getuid?.() ||
        (status.mode & 0o022) !== 0 ||
        status.size > 1_000_000
      )
        throw new AppError(
          "Agent configuration must be an ordinary file you own, under 1 MB, without shared write access.",
        );
      return await file.readFile("utf8");
    } finally {
      await file.close();
    }
  } catch (error) {
    if (isMissing(error)) return undefined;
    if (error instanceof Error && "code" in error && error.code === "ELOOP")
      throw new AppError("Agent configuration cannot be a symbolic link.");
    throw error;
  }
}

export async function writeAtomic(
  path: string,
  content: string,
  mode = 0o600,
): Promise<void> {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, content, { flag: "wx", mode });
    await rename(temporary, path);
  } finally {
    await rm(temporary, { force: true });
  }
}

const object = z.record(z.string(), z.json());
export function jsonObject(content: string): JsonObject {
  try {
    return object.parse(JSON.parse(content));
  } catch {
    throw new AppError(
      "This agent configuration is not a valid JSON object. Fix it before connecting.",
    );
  }
}

// Retain only values equal to our generated, nonsecret settings. Never back up
// arbitrary user configuration: it may contain credentials.
export function matchingClaudeConfig(
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

// Only fields installed by this app are eligible for replacement or removal.
// A collision is an error, so existing credentials need no plaintext backup.
export function mergeNativeConfig(
  agent: Agent,
  current: string | undefined,
  previous: string | undefined,
  next: string | undefined,
): string | undefined {
  const conflict = () => {
    throw new AppError(
      "Agent configuration already exists or was edited outside nonstopvibin. Resolve the conflicting settings before reconnecting or disconnecting.",
      409,
    );
  };
  if (agent === "codex") {
    const split = (content: string | undefined) => {
      const preferences: string[] = [];
      const state: string[] = [];
      let table = false;
      let runtimeState = false;
      const connection = content
        ?.split("\n")
        .filter((line) => {
          if (/^\s*\[/.test(line)) {
            table = true;
            // Codex records hook approvals and model-picker notices here.
            runtimeState =
              /^\s*\[(?:hooks\.state(?:\."(?:[^"\\]|\\.)*")?|tui\.model_availability_nux)\]\s*(?:#.*)?$/.test(
                line,
              );
          }
          if (runtimeState) {
            state.push(line);
            return false;
          }
          // Native /model owns scalar preferences, not provider/auth routing.
          if (
            !table &&
            /^\s*(?:model|review_model|model_reasoning_effort|model_reasoning_summary|model_verbosity)\s*=\s*(?:"(?:[^"\\]|\\.)*"|'[^']*')\s*(?:#.*)?$/.test(
              line,
            )
          ) {
            preferences.push(line);
            return false;
          }
          return true;
        })
        .join("\n")
        .trim();
      return { connection, preferences, state };
    };
    const saved = split(current);
    const old = split(previous);
    if (
      current !== undefined &&
      saved.connection !== old.connection &&
      saved.connection !== split(next).connection
    )
      conflict();
    if (current !== undefined && previous === next) return current;
    const preferences = saved.preferences.filter(
      (line) => !old.preferences.includes(line),
    );
    const result = [
      ...preferences,
      ...(next === undefined ? [] : [next]),
      ...saved.state,
    ].join("\n");
    return result || undefined;
  }
  if (agent !== "claude") {
    if (current !== undefined && current !== previous && current !== next)
      conflict();
    return next;
  }
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

export async function replaceConfig(
  path: string,
  before: string | undefined,
  after: string | undefined,
): Promise<void> {
  if ((await readConfig(path)) !== before)
    throw new AppError(
      "Agent configuration changed during setup. Try again.",
      409,
    );
  if (after === undefined) await rm(path, { force: true });
  else await writeAtomic(path, after);
}

export async function projectDirectory(path: string): Promise<string> {
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
