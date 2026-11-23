import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import release from "../crates/core/core-release.json" with { type: "json" };
import { checkCoreDirectory } from "../scripts/core-binary.ts";

const project = resolve(import.meta.dirname, "..");

test("packaging rejects an unreviewed core, wrong target and altered binary", async () => {
  const root = await mkdtemp(join(tmpdir(), "nv-core-package-"));
  try {
    const core = join(root, ".vendor/core");
    await mkdir(core, { recursive: true });
    const installedManifest = JSON.parse(
      await readFile(join(project, ".vendor/core/manifest.json"), "utf8"),
    );
    const bytes = await readFile(join(project, ".vendor/core/cli-proxy-api"));
    await writeFile(join(core, "cli-proxy-api"), bytes);
    const target = `${installedManifest.platform}_${installedManifest.arch}`;
    const manifest = {
      ...installedManifest,
      version: release.version,
      sha256: release.checksums[target],
      binarySha256: release.binaries[target],
    };
    const save = (value) =>
      writeFile(join(core, "manifest.json"), JSON.stringify(value));
    const { platform, arch } = installedManifest;
    const check = (target = platform, targetArch = arch) =>
      checkCoreDirectory(core, target, targetArch);
    await save(manifest);
    check();
    const synthetic = Buffer.from("synthetic core");
    await writeFile(join(core, "cli-proxy-api"), synthetic);
    await save({
      ...manifest,
      binarySha256: createHash("sha256").update(synthetic).digest("hex"),
    });
    assert.throws(check, /reviewed binary pin/);
    await writeFile(join(core, "cli-proxy-api"), bytes);
    await save(manifest);
    assert.throws(
      () => check(platform === "darwin" ? "linux" : "darwin"),
      /target/,
    );
    assert.throws(
      () => check(platform, arch === "arm64" ? "x64" : "arm64"),
      /target/,
    );
    await save({ ...manifest, version: "0.0.0" });
    assert.throws(check, /reviewed release/);
    await save({ ...manifest, sha256: "0".repeat(64) });
    assert.throws(check, /reviewed release/);
    await save(manifest);
    await writeFile(join(core, "cli-proxy-api"), "tampered");
    assert.throws(check, /reviewed release pin/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
