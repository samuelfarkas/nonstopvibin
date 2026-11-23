// What the Rust packaging scripts (package-mac.ts, package-linux.ts, package-server.ts) share:
// the cargo build, the core tool, the verified core, the licences and the electron-updater feed.
import {
  execFileSync,
  spawnSync,
  type ExecFileSyncOptions,
} from "node:child_process";
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
import { clientDirectory, rustLicenses, uiLicences } from "./distribution.ts";
import { sha256, sha512 } from "./pinned-download.ts";
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

/** Runs `bun run core:tool` (crates/core/examples/core-tool.rs); answers what it printed. */
export function coreTool(...args: string[]): string {
  const result = spawnSync("bun", ["run", "--silent", "core:tool", ...args], {
    cwd: repo,
    encoding: "utf8",
  });
  if (result.status !== 0)
    throw new Error(
      result.stderr.trim() || `core:tool ${args.join(" ")} failed`,
    );
  return result.stdout;
}

/** The core's file names and the bundled pin's version, as crates/core/src/binary.rs names them. */
export const coreLayout: {
  /** Relative to the packaged app's resources and the server archive. */
  packaged: string;
  binary: string;
  manifest: string;
  license: string;
  version: string;
} = JSON.parse(coreTool("layout"));
export const coreFiles = [coreLayout.binary, coreLayout.manifest];

/**
 * Installs the pinned core for `platform`/`arch` under `work` (so the checkout's .vendor/core
 * stays as it is) and copies the files that ship to `destination`. Answers the path of the
 * release's licence, which stageLicences ships.
 */
export async function stageCore(
  work: string,
  destination: string,
  platform: string,
  arch: string,
): Promise<string> {
  const core = join(work, "core-stage");
  coreTool("install", "--target", `${platform}_${arch}`, core);
  await mkdir(destination, { recursive: true });
  for (const file of coreFiles)
    await copyFile(join(core, file), join(destination, file));
  return join(core, coreLayout.license);
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

/**
 * Writes the licence folder every Rust package ships (distribution.ts `rustLicenses`);
 * `coreLicence` is the staged core's, from stageCore.
 */
export async function stageLicences(
  destination: string,
  builds: RustBuild[],
  coreLicence: string,
): Promise<void> {
  await stat(join(repo, uiLicences)).catch(() => {
    throw new Error(`Missing ${uiLicences}; run bun run build:client first.`);
  });
  await mkdir(destination, { recursive: true });
  const generated = new Map([
    ["THIRD-PARTY-RUST.txt", rustLicenseText(builds)],
    ["Rust-std.html", rustStdNotices()],
    ["CLIProxyAPI.txt", await readFile(coreLicence, "utf8")],
  ]);
  for (const [name, source] of Object.entries(rustLicenses)) {
    const target = join(destination, name);
    const text = generated.get(name);
    if (source) await copyFile(join(repo, source), target);
    else if (text !== undefined) await writeFile(target, text);
    else throw new Error(`No source for the licence file ${name}`);
  }
}

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
