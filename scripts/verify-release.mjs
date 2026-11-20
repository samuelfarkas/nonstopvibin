// `bun scripts/verify-release.mjs <tag> [assets]`: checks the tag against package.json, then
// the assets when a directory is given, and prints the tag's channel (release or prerelease).
// Tag mode needs no installed dependencies; the release preflight runs it before `bun ci`.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, readdir } from "node:fs/promises";
import { basename, join } from "node:path";
import pkg from "../package.json" with { type: "json" };
import { releaseChannel } from "../src/shared/release-channel.ts";
import { serverArchiveName, serverArchitectures } from "./distribution.ts";

// The file electron-updater downloads on each platform with an update feed.
const updatePayload = { mac: "zip", linux: "AppImage" };

export function verifyReleaseTag(tag, manifest, repository) {
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
  const feed = manifest.build.publish[0];
  assert.equal(feed.provider, "github");
  assert.equal(
    `${feed.owner}/${feed.repo}`,
    repository,
    "Update feed must match the publishing repository",
  );
  return channel;
}

/** Checks `directory` holds every asset `manifest` (package.json) builds for its version. */
export async function verifyReleaseAssets(directory, manifest) {
  const { default: YAML } = await import("yaml");
  const { version, build } = manifest;
  const names = await readdir(directory);
  // electron-builder names each installer with its target as the extension.
  for (const [platform, payload] of Object.entries(updatePayload)) {
    const targets = build[platform].target;
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
  // electron-builder 26 uses latest manifests for GitHub, including prereleases.
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
        createHash("sha512").update(bytes).digest("base64"),
        file.sha512,
        `Checksum mismatch: ${file.url}`,
      );
    }
  }
}

if (import.meta.main) {
  const channel = verifyReleaseTag(
    process.argv[2],
    pkg,
    process.env.GITHUB_REPOSITORY,
  );
  if (process.argv[3]) await verifyReleaseAssets(process.argv[3], pkg);
  console.log(channel);
}
