import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import YAML from "yaml";
import {
  verifyReleaseTag,
  verifyReleaseAssets,
} from "../scripts/verify-release.mjs";
import {
  serverArchiveName,
  serverArchitectures,
} from "../scripts/distribution.ts";
import pkg from "../package.json" with { type: "json" };

test("release tags reject disabled updates, wrong versions and wrong destinations", () => {
  const manifest = {
    version: "0.1.2",
    updatesEnabled: true,
    build: { publish: [{ provider: "github", owner: "fixture", repo: "app" }] },
  };
  assert.equal(verifyReleaseTag("v0.1.2", manifest, "fixture/app"), "release");
  assert.throws(
    () => verifyReleaseTag("v0.1.3", manifest, "fixture/app"),
    /version/,
  );
  assert.throws(
    () =>
      verifyReleaseTag(
        "v0.1.2",
        { ...manifest, updatesEnabled: false },
        "fixture/app",
      ),
    /enable/,
  );
  assert.throws(
    () => verifyReleaseTag("v0.1.2", manifest, "another/app"),
    /repository/,
  );
  assert.equal(
    verifyReleaseTag(
      "v0.2.0-beta.1",
      { ...manifest, version: "0.2.0-beta.1" },
      "fixture/app",
    ),
    "prerelease",
  );
  for (const tag of ["0.2.0", "v0.2.0-rc.1", "v0.2.0-beta", "v01.2.0"])
    assert.throws(
      () =>
        verifyReleaseTag(
          tag,
          { ...manifest, version: tag.replace(/^v/, "") },
          "fixture/app",
        ),
      /vX\.Y\.Z/,
    );
});

test("release publication requires every configured installer, both server archives, and matching update payloads", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-release-"));
  // The real build configuration, so a target added to package.json is required here too.
  const manifest = { ...pkg, version: "0.1.2" };
  const targets = [...pkg.build.mac.target, ...pkg.build.linux.target];
  const archives = serverArchitectures.map(
    (arch) => `${serverArchiveName("0.1.2", arch)}.tar.gz`,
  );
  try {
    const bytes = Buffer.from("synthetic installer");
    for (const target of targets)
      await writeFile(join(directory, `app.${target}`), bytes);
    for (const archive of archives)
      await writeFile(join(directory, archive), "synthetic server");
    for (const platform of ["mac", "linux"])
      await writeFile(
        join(directory, `latest-${platform}.yml`),
        YAML.stringify({
          version: "0.1.2",
          files: [
            {
              url: platform === "mac" ? "app.zip" : "app.AppImage",
              size: bytes.length,
              sha512: createHash("sha512").update(bytes).digest("base64"),
            },
          ],
        }),
      );
    await verifyReleaseAssets(directory, manifest);
    const later = serverArchitectures.map(
      (arch) => `${serverArchiveName("0.1.3", arch)}.tar.gz`,
    );
    for (const archive of later)
      await writeFile(join(directory, archive), "synthetic server");
    await assert.rejects(
      verifyReleaseAssets(directory, { ...manifest, version: "0.1.3" }),
      /Update manifest version mismatch/,
    );
    for (const archive of later) await rm(join(directory, archive));
    await assert.rejects(
      verifyReleaseAssets(directory, {
        ...manifest,
        build: {
          ...pkg.build,
          linux: {
            ...pkg.build.linux,
            target: [...pkg.build.linux.target, "rpm"],
          },
        },
      }),
      /Missing rpm installer/,
    );
    for (const archive of archives) {
      await rm(join(directory, archive));
      await assert.rejects(
        verifyReleaseAssets(directory, manifest),
        /Missing server archive/,
      );
      await writeFile(join(directory, archive), "synthetic server");
    }
    for (const target of targets) {
      await rm(join(directory, `app.${target}`));
      await assert.rejects(
        verifyReleaseAssets(directory, manifest),
        new RegExp(`Missing ${target} installer`),
      );
      await writeFile(join(directory, `app.${target}`), bytes);
    }
    await writeFile(
      join(directory, "app.zip"),
      Buffer.from("corrupted installer"),
    );
    await assert.rejects(verifyReleaseAssets(directory, manifest), /mismatch/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
