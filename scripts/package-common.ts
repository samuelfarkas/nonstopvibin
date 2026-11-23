// What the Rust packaging scripts (package-mac.ts, package-linux.ts, package-server.ts) share:
// the cargo build, the verified core, the licences and the electron-updater feed.
import { execFileSync, type ExecFileSyncOptions } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFile,
  cp,
  mkdir,
  readFile,
  stat,
  writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import YAML from "yaml";
import pkg from "../package.json" with { type: "json" };
import { checkCoreDirectory, coreFiles, coreLayout } from "./core-binary.ts";
import { clientDirectory } from "./distribution.ts";
import {
  rustLicenseText,
  rustStdNotices,
  type RustBuild,
} from "./rust-licenses.ts";

export const repo = resolve(import.meta.dirname, "..");
export const version = pkg.version;
export const releaseDir = join(repo, "release");

export function run(
  command: string,
  args: string[],
  options: ExecFileSyncOptions = {},
): void {
  execFileSync(command, args, { cwd: repo, stdio: "inherit", ...options });
}

/** Builds `packages` in release mode for `target` and returns the folder with the binaries. */
export function cargoBuild(
  { target, packages }: RustBuild,
  env: Record<string, string> = {},
): string {
  run(
    "cargo",
    [
      "build",
      "--release",
      "--locked",
      "--target",
      target,
      ...packages.flatMap((name) => ["-p", name]),
    ],
    { env: { ...process.env, ...env } },
  );
  return join(
    resolve(repo, process.env.CARGO_TARGET_DIR ?? "target"),
    target,
    "release",
  );
}

/**
 * Installs the pinned core for `platform`/`arch` with `core:install` in `work` (its own cwd, so
 * the checkout's .vendor/core stays as it is), checks it against the pin and copies its files
 * to `destination`.
 */
export async function stageCore(
  work: string,
  destination: string,
  platform: string,
  arch: string,
): Promise<void> {
  run("bun", [join(repo, "scripts/install-core.mjs")], {
    cwd: work,
    env: { ...process.env, CORE_PLATFORM: platform, CORE_ARCH: arch },
  });
  const core = join(work, coreLayout.checkout);
  checkCoreDirectory(core, platform, arch);
  await mkdir(destination, { recursive: true });
  for (const file of coreFiles)
    await copyFile(join(core, file), join(destination, file));
}

/** Copies the Vite build (`bun run build:client`) to `destination`, the `client/` the daemon serves. */
export async function stageClient(destination: string): Promise<void> {
  const client = join(repo, clientDirectory);
  await stat(join(client, "index.html")).catch(() => {
    throw new Error(
      `Missing ${clientDirectory}/index.html; run bun run build:client first.`,
    );
  });
  await cp(client, destination, { recursive: true });
}

/** Writes the licence folder every Rust package ships (distribution.ts `rustLicenses`). */
export async function stageLicences(
  destination: string,
  builds: RustBuild[],
): Promise<void> {
  const ui = join(repo, "dist/licenses/THIRD-PARTY.txt");
  await stat(ui).catch(() => {
    throw new Error(
      "Missing dist/licenses/THIRD-PARTY.txt; run bun run build:client first.",
    );
  });
  await mkdir(destination, { recursive: true });
  for (const [from, to] of [
    ["LICENSE", "LICENSE"],
    ["licenses/NOTICE.md", "NOTICE.md"],
    ["licenses/CLIProxyAPI.txt", "CLIProxyAPI.txt"],
    ["dist/licenses/THIRD-PARTY.txt", "THIRD-PARTY.txt"],
  ])
    await copyFile(join(repo, from), join(destination, to));
  await writeFile(
    join(destination, "THIRD-PARTY-RUST.txt"),
    rustLicenseText(builds),
  );
  await writeFile(join(destination, "Rust-std.html"), rustStdNotices());
}

export const sha512 = (bytes: Uint8Array): string =>
  createHash("sha512").update(bytes).digest("base64");
export const sha256 = (bytes: Uint8Array): string =>
  createHash("sha256").update(bytes).digest("hex");

/**
 * latest-mac.yml or latest-linux.yml in electron-builder's shape, which installed Electron
 * builds read through electron-updater. The update payload comes first. No blockmaps: an
 * Electron build that finds none downloads the whole payload.
 */
export async function writeFeed(
  directory: string,
  platform: "mac" | "linux",
  files: string[],
  releaseDate = new Date().toISOString(),
): Promise<string> {
  const entries = [];
  for (const url of files) {
    const bytes = await readFile(join(directory, url));
    entries.push({ url, sha512: sha512(bytes), size: bytes.length });
  }
  const [payload] = entries;
  if (!payload) throw new Error("An update feed needs its payload");
  const feed = join(directory, `latest-${platform}.yml`);
  // YAML 1.1 quotes the date, as electron-builder does, so js-yaml reads it as a string.
  await writeFile(
    feed,
    YAML.stringify(
      {
        version,
        files: entries,
        path: payload.url,
        sha512: payload.sha512,
        releaseDate,
      },
      { version: "1.1" },
    ),
  );
  return feed;
}

export async function report(path: string): Promise<void> {
  const bytes = await readFile(path);
  console.log(
    `${path.slice(repo.length + 1)}  ${(bytes.length / 1048576).toFixed(1)} MiB  sha256 ${sha256(bytes)}`,
  );
}
