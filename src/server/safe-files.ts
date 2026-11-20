import { constants, type Stats } from "node:fs";
import { open, rename, rm, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";

export function isMissing(cause: unknown): boolean {
  return cause instanceof Error && "code" in cause && cause.code === "ENOENT";
}

/**
 * Whether an entry is a `kind`, not a symbolic link, owned by this user, and has none of the
 * `forbidden` mode bits: 0o077 for owner-only, 0o022 for no shared write access, 0 for any mode.
 */
export function ownEntry(
  status: Stats,
  kind: "file" | "directory",
  forbidden: number,
): boolean {
  return (
    (kind === "file" ? status.isFile() : status.isDirectory()) &&
    !status.isSymbolicLink() &&
    status.uid === process.getuid?.() &&
    (status.mode & forbidden) === 0
  );
}

/**
 * Reads a file without following a symbolic link (that fails with ELOOP) or blocking on a FIFO.
 * Undefined when it is missing; throws `refused()` unless it is an ordinary file that passes
 * `ownEntry` with `forbidden` and is at most `limit` bytes.
 */
export async function readOwnFile(
  path: string,
  forbidden: number,
  limit: number,
  refused: () => Error,
): Promise<string | undefined> {
  let file;
  try {
    file = await open(
      path,
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
  } catch (error) {
    if (isMissing(error)) return undefined;
    throw error;
  }
  try {
    const status = await file.stat();
    if (!ownEntry(status, "file", forbidden) || status.size > limit)
      throw refused();
    return await file.readFile("utf8");
  } finally {
    await file.close();
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
