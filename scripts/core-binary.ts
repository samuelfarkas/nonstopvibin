import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { z } from "zod";
import coreRelease from "../crates/core/core-release.json" with { type: "json" };

// The core artifact for the build scripts (install-core, pin-core, the package scripts and
// verify-rust-package), which import this module: no build tooling here. The daemon reads the
// same pin in crates/core/src/binary.rs.

const versionSchema = z.string().regex(/^\d+\.\d+\.\d+$/);
const sha256Schema = z.string().regex(/^[a-f0-9]{64}$/);
const targetsSchema = z.object({
  darwin_arm64: sha256Schema,
  darwin_x64: sha256Schema,
  linux_arm64: sha256Schema,
  linux_x64: sha256Schema,
});
/**
 * A reviewed core release, `crates/core/core-release.json`: bundled into each build, and read
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

/** The pin this build bundles. */
export const bundledPin: CorePin = corePinSchema.parse(coreRelease);
export const coreVersion = bundledPin.version;

export const coreLayout = {
  /** Written by `bun run core:install`, relative to the checkout. */
  checkout: ".vendor/core",
  /** Relative to the packaged app's resources. */
  packaged: "core",
  binary: "cli-proxy-api",
  manifest: "manifest.json",
} as const;
export const coreFiles = [coreLayout.binary, coreLayout.manifest] as const;

const archivePins = new Map(Object.entries(bundledPin.checksums));
const binaryPins = new Map(Object.entries(bundledPin.binaries));
const matches = (
  table: Map<string, string>,
  platform: string,
  arch: string,
  sha256: string,
) => {
  const pin = table.get(`${platform}_${arch}`);
  return pin !== undefined && pin === sha256;
};
/** Whether `sha256` is the reviewed checksum of the upstream archive for this target. */
export const pinnedArchive = (
  platform: string,
  arch: string,
  sha256: string,
): boolean => matches(archivePins, platform, arch, sha256);
/** Whether `sha256` is the reviewed hash of the unsigned binary for this target. */
const pinnedBinary = (
  platform: string,
  arch: string,
  sha256: string,
): boolean => matches(binaryPins, platform, arch, sha256);

/**
 * `manifest.json` beside the binary: `sha256` is the upstream archive's checksum and
 * `binarySha256` the binary's hash, which macOS signing rewrites (scripts/package-mac.ts).
 */
const manifestSchema = z.object({
  version: z.string(),
  platform: z.string(),
  arch: z.string(),
  archive: z.string(),
  sha256: z.string(),
  binarySha256: z.string(),
});
export type CoreManifest = z.infer<typeof manifestSchema>;
export function coreManifest(
  version: string,
  platform: string,
  arch: string,
  archive: string,
  sha256: string,
  binarySha256: string,
): CoreManifest {
  return {
    version,
    platform,
    arch,
    archive,
    sha256,
    binarySha256,
  };
}
export function readCoreManifest(directory: string): CoreManifest {
  return manifestSchema.parse(
    JSON.parse(readFileSync(join(directory, coreLayout.manifest), "utf8")),
  );
}
export function writeCoreManifest(
  directory: string,
  manifest: CoreManifest,
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
