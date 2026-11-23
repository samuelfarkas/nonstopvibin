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
  desktopTargets,
  serverArchiveName,
  serverArchitectures,
} from "../scripts/distribution.ts";

test("release tags reject disabled updates, wrong versions and wrong destinations", () => {
  const manifest = { version: "0.1.2", updatesEnabled: true };
  const feed = { owner: "fixture", repo: "app" };
  const verify = (tag, value, repository = "fixture/app") =>
    verifyReleaseTag(tag, value, repository, feed);
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
