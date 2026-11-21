import { readFile } from "node:fs/promises";
import {
  createReadStream,
  existsSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { z } from "zod";
import coreRelease from "./core-release.json";
import { AppError } from "./errors.ts";
import { parse, record, type JsonObject } from "./json.ts";
import type { Json } from "../shared/types.ts";

// The core artifact for the app and for the build scripts (install-core, pin-core,
// validate-core, verify-package, sign-mac, build-server), which import this module: no build
// tooling here.

const versionSchema = z.string().regex(/^\d+\.\d+\.\d+$/);
const sha256Schema = z.string().regex(/^[a-f0-9]{64}$/);
const targetsSchema = z.object({
  darwin_arm64: sha256Schema,
  darwin_x64: sha256Schema,
  linux_arm64: sha256Schema,
  linux_x64: sha256Schema,
});
/**
 * A reviewed core release, `src/server/core-release.json`: bundled into each build, and read
 * from `main` by installed ones. Unknown keys are dropped, so a later field does not stop
 * older apps from reading it.
 */
export const corePinSchema = z.object({
  version: versionSchema,
  /** The oldest NonstopVibin version that may run this core. */
  minApp: versionSchema.optional(),
  /** Each target's upstream archive SHA-256. */
  checksums: targetsSchema,
  /** Each target's extracted, unsigned executable SHA-256. */
  binaries: targetsSchema,
});
export type CorePin = z.infer<typeof corePinSchema>;
/** `<platform>_<arch>` by Node's names. */
export type CoreTarget = keyof CorePin["binaries"];
export const coreTargets = targetsSchema.keyof().options;
export function coreTarget(
  platform: string,
  arch: string,
): CoreTarget | undefined {
  return coreTargets.find((target) => target === `${platform}_${arch}`);
}
/** The upstream release archive's name for a target. */
export function coreArchive(
  version: string,
  platform: string,
  arch: string,
): string {
  const suffix = new Map([
    ["x64", "amd64"],
    ["arm64", "aarch64"],
  ]).get(arch);
  if (!suffix) throw new Error(`Unsupported architecture: ${arch}`);
  return `CLIProxyAPI_${version}_${platform}_${suffix}.tar.gz`;
}
/** Orders X.Y.Z versions numerically; a prerelease (X.Y.Z-beta.N) sorts below its X.Y.Z. */
export function compareVersions(a: string, b: string): number {
  const parts = (version: string) => {
    const [release = "", prerelease] = version.split("-", 2);
    return [
      ...release.split(".").map(Number),
      prerelease === undefined ? 1 : 0,
    ];
  };
  const [x, y] = [parts(a), parts(b)];
  for (const [index, part] of x.entries())
    if (part !== y[index]) return part - (y[index] ?? 0);
  return 0;
}

/** The pin this build bundles. */
export const bundledPin: CorePin = corePinSchema.parse(coreRelease);
export const coreVersion = bundledPin.version;

export const coreLayout = {
  /** Written by `bun run core:install`, relative to the checkout. */
  checkout: ".vendor/core",
  /** Relative to the desktop app's resources (package.json `extraResources`). */
  packaged: "core",
  binary: "cli-proxy-api",
  manifest: "manifest.json",
} as const;
export const coreFiles = [coreLayout.binary, coreLayout.manifest] as const;

const archivePins = new Map(Object.entries(bundledPin.checksums));
const binaryPins = new Map(Object.entries(bundledPin.binaries));
const matches = (
  table: Map<string, string>,
  platform: Json | undefined,
  arch: Json | undefined,
  sha256: Json | undefined,
) => {
  const pin = table.get(`${platform}_${arch}`);
  return pin !== undefined && pin === sha256;
};
/** Whether `sha256` is the reviewed checksum of the upstream archive for this target. */
export const pinnedArchive = (
  platform: Json | undefined,
  arch: Json | undefined,
  sha256: Json | undefined,
): boolean => matches(archivePins, platform, arch, sha256);
/** Whether `sha256` is the reviewed hash of the unsigned binary for this target. */
export const pinnedBinary = (
  platform: Json | undefined,
  arch: Json | undefined,
  sha256: Json | undefined,
): boolean => matches(binaryPins, platform, arch, sha256);

/**
 * `manifest.json` beside the binary: `sha256` is the upstream archive's checksum and
 * `binarySha256` the binary's hash, which desktop signing rewrites (scripts/sign-mac.cjs).
 */
export function coreManifest(
  version: string,
  platform: string,
  arch: string,
  archive: string,
  sha256: string,
  binarySha256: string,
): JsonObject {
  return {
    version,
    platform,
    arch,
    archive,
    sha256,
    binarySha256,
  };
}
export function readCoreManifest(directory: string): JsonObject {
  return record(
    parse(readFileSync(join(directory, coreLayout.manifest), "utf8")),
  );
}
export function writeCoreManifest(
  directory: string,
  manifest: JsonObject,
): void {
  writeFileSync(
    join(directory, coreLayout.manifest),
    JSON.stringify(manifest, null, 2),
  );
}

/**
 * Throws unless the core in `directory` is the reviewed pin for `platform`/`arch`: the
 * manifest's version and archive checksum, its target, its binary hash, then the binary.
 * Packaging and the server archive build run this before shipping a core.
 */
export function checkCoreDirectory(
  directory: string,
  platform: string,
  arch: string,
): void {
  const manifest = readCoreManifest(directory);
  if (
    manifest.version !== coreVersion ||
    !pinnedArchive(manifest.platform, manifest.arch, manifest.sha256)
  )
    throw new Error("The proxy core does not match the reviewed release pin.");
  if (manifest.platform !== platform || manifest.arch !== arch)
    throw new Error(
      `The proxy core is ${manifest.platform}/${manifest.arch}, but the target is ${platform}/${arch}. Run core:install for the target first.`,
    );
  if (!coreTarget(platform, arch))
    throw new Error("The proxy core target has no reviewed binary pin.");
  if (!pinnedBinary(platform, arch, manifest.binarySha256))
    throw new Error(
      "The proxy core manifest does not match the reviewed binary pin. Run bun run core:install again.",
    );
  const digest = createHash("sha256")
    .update(readFileSync(join(directory, coreLayout.binary)))
    .digest("hex");
  if (!pinnedBinary(platform, arch, digest))
    throw new Error(
      "The proxy core binary does not match the reviewed release pin. Run bun run core:install again.",
    );
}

/** A file's SHA-256, streamed: a core is too large to read into one buffer. */
export async function fileSha256(path: string): Promise<string> {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  return digest.digest("hex");
}

/**
 * Checks the core binary against the `manifest.json` beside it and, when `requirePinnedCore`
 * is set, against the binary hash this build pins for the current platform.
 */
export async function verifyCore(
  binary: string,
  requirePinnedCore = false,
): Promise<void> {
  if (!existsSync(binary))
    throw new AppError(
      `The proxy core is missing at ${binary}. Reinstall NonstopVibin.`,
      503,
    );
  const manifest = record(
    parse(await readFile(join(binary, "..", coreLayout.manifest), "utf8")),
  );
  const hash = await fileSha256(binary);
  // Headless releases must also match the compiled pin. Desktop signing
  // changes the binary and refreshes its manifest after packaging verifies it.
  if (
    manifest.binarySha256 !== hash ||
    (requirePinnedCore && !pinnedBinary(process.platform, process.arch, hash))
  )
    throw new AppError(
      "The proxy core failed its integrity check. Reinstall NonstopVibin.",
      503,
    );
}
