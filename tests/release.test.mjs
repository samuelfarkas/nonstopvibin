import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import YAML from "yaml";
import {
  verifyManifests,
  verifyReleaseTag,
  verifyReleaseAssets,
} from "../scripts/verify-release.mjs";
import {
  desktopTargets,
  serverArchiveName,
  serverArchitectures,
} from "../scripts/distribution.ts";
import { releaseFiles, writeSums } from "../scripts/sign-release.ts";

const cargo = {
  version: "0.1.2",
  repository: "https://github.com/fixture/app",
};
const manifest = {
  version: "0.1.2",
  updatesEnabled: true,
  homepage: "https://github.com/fixture/app",
  repository: { type: "git", url: "git+https://github.com/fixture/app.git" },
  bugs: { url: "https://github.com/fixture/app/issues" },
};

test("package.json must name Cargo.toml's version and repository", () => {
  verifyManifests(manifest, cargo);
  assert.throws(
    () => verifyManifests({ ...manifest, version: "0.1.3" }, cargo),
    /versions differ/,
  );
  for (const changed of [
    { homepage: "https://github.com/fixture/other" },
    { repository: { url: "git+https://github.com/fixture/other.git" } },
    { bugs: { url: "https://github.com/fixture/other/issues" } },
  ])
    assert.throws(
      () => verifyManifests({ ...manifest, ...changed }, cargo),
      /repositories differ/,
    );
});

test("release tags reject disabled updates, wrong versions and wrong destinations", () => {
  const verify = (tag, value, repository = "fixture/app") =>
    verifyReleaseTag(
      tag,
      value,
      { ...cargo, version: value.version },
      repository,
    );
  assert.equal(verify("v0.1.2", manifest), "release");
  assert.throws(() => verify("v0.1.3", manifest), /version/);
  assert.throws(
    () => verify("v0.1.2", { ...manifest, updatesEnabled: false }),
    /enable/,
  );
  assert.throws(() => verify("v0.1.2", manifest, "another/app"), /repository/);
  assert.equal(
    verify("v0.2.0-beta.1", { ...manifest, version: "0.2.0-beta.1" }),
    "prerelease",
  );
  for (const tag of ["0.2.0", "v0.2.0-rc.1", "v0.2.0-beta", "v01.2.0"])
    assert.throws(
      () => verify(tag, { ...manifest, version: tag.replace(/^v/, "") }),
      /vX\.Y\.Z/,
    );
});

test("release publication requires every configured installer, both server archives, and matching update payloads", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-release-"));
  // The real targets, so a target added to scripts/distribution.ts is required here too.
  const targets = [...desktopTargets.mac, ...desktopTargets.linux];
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
    await verifyReleaseAssets(directory, "0.1.2");
    const later = serverArchitectures.map(
      (arch) => `${serverArchiveName("0.1.3", arch)}.tar.gz`,
    );
    for (const archive of later)
      await writeFile(join(directory, archive), "synthetic server");
    await assert.rejects(
      verifyReleaseAssets(directory, "0.1.3"),
      /Update manifest version mismatch/,
    );
    for (const archive of later) await rm(join(directory, archive));
    await assert.rejects(
      verifyReleaseAssets(directory, "0.1.2", {
        ...desktopTargets,
        linux: [...desktopTargets.linux, "rpm"],
      }),
      /Missing rpm installer/,
    );
    for (const archive of archives) {
      await rm(join(directory, archive));
      await assert.rejects(
        verifyReleaseAssets(directory, "0.1.2"),
        /Missing server archive/,
      );
      await writeFile(join(directory, archive), "synthetic server");
    }
    for (const target of targets) {
      await rm(join(directory, `app.${target}`));
      await assert.rejects(
        verifyReleaseAssets(directory, "0.1.2"),
        new RegExp(`Missing ${target} installer`),
      );
      await writeFile(join(directory, `app.${target}`), bytes);
    }
    await writeFile(
      join(directory, "app.zip"),
      Buffer.from("corrupted installer"),
    );
    await assert.rejects(verifyReleaseAssets(directory, "0.1.2"), /mismatch/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("SHA256SUMS lists the release files, so writing it again gives the same manifest", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-sums-"));
  try {
    await writeFile(join(directory, "b.yml"), "b");
    await writeFile(join(directory, "a.zip"), "a");
    // What a local dist leaves beside the installers, and an earlier run's output.
    await mkdir(join(directory, "mac-arm64"));
    await writeFile(join(directory, "SHA256SUMS"), "stale\n");
    await writeFile(join(directory, "SHA256SUMS.minisig"), "stale\n");
    await writeSums(directory, await releaseFiles(directory));
    assert.equal(
      await readFile(join(directory, "SHA256SUMS"), "utf8"),
      "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb  a.zip\n" +
        "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d  b.yml\n",
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
