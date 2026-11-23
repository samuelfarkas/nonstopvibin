// `bun scripts/verify-release.mjs [<tag> [assets]]`: checks that package.json and Cargo.toml
// state the same version and repository (all `bun run quality` asks), then the tag against them
// and the assets when a directory is given, and prints the tag's channel (release or
// prerelease). Tag mode needs no installed dependencies; the release preflight runs it before
// `bun ci`.
import assert from "node:assert/strict";
import { TOML } from "bun";
import { readFile, readdir } from "node:fs/promises";
import { basename, join } from "node:path";
import pkg from "../package.json" with { type: "json" };
import {
  desktopTargets,
  serverArchiveName,
  serverArchitectures,
  updatePayload,
} from "./distribution.ts";
import { sha512 } from "./pinned-download.ts";
import { releaseChannel } from "./release-channel.ts";

/** Cargo.toml's `[workspace.package]`: the version and repository the binaries are built with. */
export async function cargoPackage(
  path = join(import.meta.dirname, "../Cargo.toml"),
) {
  return TOML.parse(await readFile(path, "utf8")).workspace.package;
}

/** package.json names the version and repository Cargo.toml builds the updater with. */
export function verifyManifests(manifest, cargo) {
  assert.equal(
    manifest.version,
    cargo.version,
    "package.json and Cargo.toml versions differ",
  );
  const repository = cargo.repository;
  assert.deepEqual(
    [manifest.repository?.url, manifest.homepage, manifest.bugs?.url],
    [`git+${repository}.git`, repository, `${repository}/issues`],
    "package.json and Cargo.toml repositories differ",
  );
}

export function verifyReleaseTag(tag, manifest, cargo, repository) {
  verifyManifests(manifest, cargo);
  const channel = tag.startsWith("v") && releaseChannel(tag.slice(1));
  assert.ok(channel, "Use vX.Y.Z or vX.Y.Z-beta.N");
  assert.equal(
    tag,
    `v${manifest.version}`,
    "Tag must match package.json version",
  );
  assert.equal(
    manifest.updatesEnabled,
    true,
    "Configure the release repository and enable updatesEnabled first",
  );
  assert.equal(
    cargo.repository,
    `https://github.com/${repository}`,
    "Update feed must match the publishing repository",
  );
  return channel;
}

export async function verifyReleaseAssets(
  directory,
  version,
  installers = desktopTargets,
) {
  const { default: YAML } = await import("yaml");
  const names = await readdir(directory);
  // Each installer's name ends with its target as the extension.
  for (const [platform, payload] of Object.entries(updatePayload)) {
    const targets = installers[platform];
    assert.ok(targets.includes(payload), `${platform} must build ${payload}`);
    for (const target of targets)
      assert.ok(
        names.some((name) => name.endsWith(`.${target}`)),
        `Missing ${target} installer`,
      );
  }
  for (const arch of serverArchitectures) {
    const archive = `${serverArchiveName(version, arch)}.tar.gz`;
    assert.ok(names.includes(archive), `Missing server archive ${archive}`);
  }
  // Installed builds read latest-*.yml for GitHub releases, including prereleases.
  for (const [platform, payload] of Object.entries(updatePayload)) {
    const feed = YAML.parse(
      await readFile(join(directory, `latest-${platform}.yml`), "utf8"),
    );
    assert.equal(feed.version, version, "Update manifest version mismatch");
    assert.ok(
      Array.isArray(feed.files) && feed.files.length,
      "Missing update files",
    );
    assert.ok(
      feed.files.some((file) => file.url.endsWith(`.${payload}`)),
      "Missing auto-update payload",
    );
    for (const file of feed.files) {
      assert.equal(
        file.url,
        basename(file.url),
        "Update payload must be a release asset filename",
      );
      const bytes = await readFile(join(directory, file.url));
      assert.equal(bytes.length, file.size, `Size mismatch: ${file.url}`);
      assert.equal(
        sha512(bytes),
        file.sha512,
        `Checksum mismatch: ${file.url}`,
      );
    }
  }
}

if (import.meta.main) {
  const [tag, assets] = process.argv.slice(2);
  const cargo = await cargoPackage();
  if (!tag) verifyManifests(pkg, cargo);
  else {
    const channel = verifyReleaseTag(
      tag,
      pkg,
      cargo,
      process.env.GITHUB_REPOSITORY,
    );
    if (assets) await verifyReleaseAssets(assets, pkg.version);
    console.log(channel);
  }
}
