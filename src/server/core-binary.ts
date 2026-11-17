import { readFile } from "node:fs/promises";
import { createReadStream, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import coreRelease from "./core-release.json";
import { AppError } from "./errors.ts";
import { parse, record, type JsonObject } from "./json.ts";
import type { Json } from "../shared/types.ts";

// The core artifact for the app and for the build scripts (install-core, validate-core,
// verify-package, sign-mac, build-server), which import this module: no build tooling here.

export const coreVersion = coreRelease.version;

export const coreLayout = {
  /** Written by `bun run core:install`, relative to the checkout. */
  checkout: ".vendor/core",
  /** Relative to the desktop app's resources (package.json `extraResources`). */
  packaged: "core",
  binary: "cli-proxy-api",
  manifest: "manifest.json",
} as const;
export const coreFiles = [coreLayout.binary, coreLayout.manifest] as const;

const archivePins = new Map(Object.entries(coreRelease.checksums));
const binaryPins = new Map(Object.entries(coreRelease.binaries));
const matches = (
  table: Map<string, string>,
  platform: Json | undefined,
  arch: Json | undefined,
  sha256: Json | undefined,
) => {
  const pin = table.get(`${platform}_${arch}`);
  return pin !== undefined && pin === sha256;
};
export const pinned = (platform: string, arch: string): boolean =>
  binaryPins.has(`${platform}_${arch}`);
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
  platform: string,
  arch: string,
  archive: string,
  sha256: string,
  binarySha256: string,
): JsonObject {
  return {
    version: coreVersion,
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
  if (!pinned(platform, arch))
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

/**
 * Checks the core binary against the `manifest.json` beside it and, when `requirePinnedCore`
 * is set, against the binary hash this build pins for the current platform.
 */
export async function verifyCore(
  binary: string,
  requirePinnedCore = false,
): Promise<void> {
  const manifest = record(
    parse(await readFile(join(binary, "..", coreLayout.manifest), "utf8")),
  );
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(binary)) digest.update(chunk);
  const hash = digest.digest("hex");
  // Headless releases must also match the compiled pin. Desktop signing
  // changes the binary and refreshes its manifest after packaging verifies it.
  if (
    manifest.binarySha256 !== hash ||
    (requirePinnedCore && !pinnedBinary(process.platform, process.arch, hash))
  )
    throw new AppError("The bundled core failed its integrity check.", 503);
}
