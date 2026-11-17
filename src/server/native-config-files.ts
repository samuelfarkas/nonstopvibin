import { lstat, mkdir, realpath, rm } from "node:fs/promises";
import { join } from "node:path";
import { z } from "zod";
import { AppError } from "./errors.ts";
import type { JsonObject } from "./json.ts";
import { isMissing, ownEntry, readOwnFile, writeAtomic } from "./safe-files.ts";

// Reading and replacing files in the user's agent configuration, with the refusals
// that keep a symlinked, shared or foreign-owned file from being read or replaced.

// The user selects the root; generated descendants must never traverse symlinks.
export async function configPath(
  root: string,
  names: string[],
  create: boolean,
): Promise<string> {
  const canonical = await realpath(root);
  const validateDirectory = async (folder: string) => {
    if (!ownEntry(await lstat(folder), "directory", 0o022))
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
    return await readOwnFile(
      path,
      0o022,
      1_000_000,
      () =>
        new AppError(
          "Agent configuration must be an ordinary file you own, under 1 MB, without shared write access.",
        ),
    );
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ELOOP")
      throw new AppError("Agent configuration cannot be a symbolic link.");
    throw error;
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
