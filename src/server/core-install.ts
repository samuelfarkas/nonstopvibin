import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { open, rm } from "node:fs/promises";
import { basename, join } from "node:path";
import { AppError, errorMessage } from "./errors.ts";
import {
  coreArchive,
  coreLayout,
  coreManifest,
  coreTarget,
  writeCoreManifest,
  type CorePin,
} from "./core-binary.ts";

// Installs a reviewed core release, for `bun run core:install` and for core updates in
// installed builds; `bun run core:pin` hashes upstream releases with the same helpers.

/** Where upstream publishes each release's archives, under `v<version>/`. */
export const coreArchives =
  "https://github.com/router-for-me/CLIProxyAPI/releases/download";
/** Above any core archive or executable; executables are about 75 MB. */
const sizeLimit = 256 * 1024 * 1024;
const downloadTimeout = 10 * 60_000;

/** Writes `chunks` to a new file at `path` with `mode`; answers their SHA-256. */
async function save(
  chunks: AsyncIterable<Uint8Array>,
  path: string,
  mode: number,
): Promise<string> {
  const hash = createHash("sha256");
  const file = await open(path, "wx", mode);
  try {
    // The mode is exact whatever the umask: packaging copies the executable's.
    await file.chmod(mode);
    let size = 0;
    for await (const chunk of chunks) {
      size += chunk.byteLength;
      if (size > sizeLimit)
        throw new AppError(`${basename(path)} is larger than any core.`, 502);
      hash.update(chunk);
      await file.write(chunk);
    }
  } finally {
    await file.close();
  }
  return hash.digest("hex");
}

/** A response body's chunks; stopping early cancels the response. */
async function* chunks(body: ReadableStream<Uint8Array>) {
  const reader = body.getReader();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      yield value;
    }
  } finally {
    await reader.cancel();
  }
}

/** Streams `url` into a new owner-only file at `path`; answers its SHA-256. */
export async function download(
  url: string,
  path: string,
  signal?: AbortSignal,
): Promise<string> {
  const name = basename(new URL(url).pathname);
  const timeout = AbortSignal.timeout(downloadTimeout);
  try {
    const response = await fetch(url, {
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    if (!response.ok || !response.body)
      throw new AppError(
        `Could not download ${name} (HTTP ${response.status}).`,
        502,
      );
    return await save(chunks(response.body), path, 0o600);
  } catch (error) {
    signal?.throwIfAborted();
    if (error instanceof AppError) throw error;
    throw new AppError(
      `Could not download ${name}: ${errorMessage(error)}`,
      502,
    );
  }
}

/**
 * Streams `member` of the gzipped tar `archive` into a new file at `path` with `mode`;
 * answers its SHA-256. Nothing else of the archive reaches the disk.
 */
export async function extract(
  archive: string,
  member: string,
  path: string,
  mode: number,
  signal?: AbortSignal,
): Promise<string> {
  const tar = spawn("tar", ["-xzOf", archive, member], {
    stdio: ["ignore", "pipe", "ignore"],
    signal,
  });
  // Rejects when tar cannot start or is aborted.
  const closed = once(tar, "close");
  let sha256;
  try {
    sha256 = await save(tar.stdout, path, mode);
  } catch (error) {
    tar.kill("SIGKILL");
    // The failure that stopped the copy is the one to report.
    await closed.catch(() => undefined);
    throw error;
  }
  const [code] = await closed;
  if (code !== 0) throw new AppError(`The core archive has no ${member}.`, 502);
  return sha256;
}

export interface InstallOptions {
  /** Where the release archives are downloaded from; tests serve their own. */
  archives?: string;
  /** Also extracts the release's LICENSE. */
  license?: boolean;
  signal?: AbortSignal;
}

/**
 * Installs the `pin`'s core for `platform`/`arch` into `directory` as `verifyCore` reads it:
 * the executable and its manifest. Throws unless the archive and the executable both match
 * the pin, leaving the directory for its caller to discard; the archive is deleted either way.
 */
export async function installCore(
  pin: CorePin,
  platform: string,
  arch: string,
  directory: string,
  { archives = coreArchives, license = false, signal }: InstallOptions = {},
): Promise<void> {
  const target = coreTarget(platform, arch);
  if (!target)
    throw new AppError("Supported core platforms: macOS/Linux arm64 and x64.");
  const name = coreArchive(pin.version, platform, arch);
  const archive = join(directory, name);
  const mismatch = () =>
    new AppError(
      "The downloaded core does not match the reviewed pin. Nothing was changed.",
      502,
    );
  try {
    const sha256 = await download(
      `${archives}/v${pin.version}/${name}`,
      archive,
      signal,
    );
    if (sha256 !== pin.checksums[target]) throw mismatch();
    const binarySha256 = await extract(
      archive,
      coreLayout.binary,
      join(directory, coreLayout.binary),
      0o755,
      signal,
    );
    if (binarySha256 !== pin.binaries[target]) throw mismatch();
    if (license)
      await extract(
        archive,
        "LICENSE",
        join(directory, "LICENSE"),
        0o644,
        signal,
      );
    writeCoreManifest(
      directory,
      coreManifest(pin.version, platform, arch, name, sha256, binarySha256),
    );
  } finally {
    await rm(archive, { force: true });
  }
}
