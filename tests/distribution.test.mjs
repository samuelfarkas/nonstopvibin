import test from "node:test";
import YAML from "yaml";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, readFile, cp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { checkCoreDirectory } from "../scripts/core-binary.ts";
import release from "../crates/core/core-release.json" with { type: "json" };

const project = resolve(import.meta.dirname, "..");

test("Git ignores private state while retaining source, skill copies and lockfiles", async () => {
  const root = await mkdtemp(join(tmpdir(), "nv-ignore-"));
  try {
    await cp(join(project, ".gitignore"), join(root, ".gitignore"));
    await mkdir(join(root, ".deepsec"));
    await cp(
      join(project, ".deepsec/.gitignore"),
      join(root, ".deepsec/.gitignore"),
    );
    execFileSync("git", ["init", "--quiet", root]);
    const ignored = [
      ".runtime/auth/account.json",
      ".test-runtime/demo/key",
      ".playwright-cli/page.yml",
      ".codex/state.json",
      ".env",
      ".env.production",
      "credentials.json",
      "accounts/auth.json",
      "id_ed25519",
      "data.sqlite-wal",
      "report.sarif",
      "release/app.dmg",
      "artifacts/scan.json",
      ".deepsec/data/app/files/source.json",
      ".deepsec/data/app/runs/run.json",
      ".deepsec/data/app/project.json",
    ];
    const kept = [
      "src/main.tsx",
      "bun.lock",
      "crates/core/core-release.json",
      ".env.example",
      ".deepsec/.env.example",
      ".deepsec/bun.lock",
      ".deepsec/data/app/INFO.md",
      ".deepsec/data/app/config.json",
      ".agents/skills/vite/SKILL.md",
      "docs/media/subscriptions.png",
    ];
    const actual = execFileSync(
      "git",
      ["-C", root, "check-ignore", "--no-index", "--stdin"],
      { input: [...ignored, ...kept].join("\n") + "\n", encoding: "utf8" },
    )
      .trim()
      .split("\n");
    assert.deepEqual(actual, ignored);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

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

const workflow = async (name) =>
  YAML.parse(await readFile(join(project, ".github/workflows", name), "utf8"));

test("tag publication requires dependency and secret checks for the release revision", async () => {
  const release = await workflow("release.yml");
  assert.ok(release.jobs.release.needs.includes("security"));
  assert.equal(release.jobs.security.uses, "./.github/workflows/security.yml");
  assert.equal(release.jobs.security.if, undefined);
  const security = await workflow("security.yml");
  assert.ok(Object.hasOwn(security.on, "workflow_call"));
  const jobs = Object.values(security.jobs);
  for (const job of jobs) {
    assert.equal(job.if, undefined);
    assert.ok(!job["continue-on-error"]);
  }
  const history = jobs.find((job) =>
    job.steps.some((step) => step.run === "bun run security:history"),
  );
  assert.equal(
    history.steps.find((step) => step.uses?.startsWith("actions/checkout@"))
      .with["fetch-depth"],
    0,
  );
  const steps = jobs.flatMap((job) => job.steps);
  for (const command of [
    "bun run security:deps",
    "bun run security:tooling",
    "bun run security:secrets",
    "bun run security:history",
    "cargo deny --locked check",
    "cargo audit",
  ])
    assert.ok(
      steps.some((step) => step.run === command && !step["continue-on-error"]),
    );
});

test("a release publishes only after the pull-request checks pass for its revision", async () => {
  const [build, release] = await Promise.all(
    ["build.yml", "release.yml"].map(workflow),
  );
  const checks = "./.github/workflows/checks.yml";
  assert.equal(build.jobs.build.uses, checks);
  assert.equal(build.jobs.build.with, undefined);
  assert.equal(release.jobs.build.uses, checks);
  assert.deepEqual(release.jobs.build.with, { release: true });
  assert.ok(release.jobs.release.needs.includes("build"));
});

test("signing secrets reach only release packaging, and releases sign SHA256SUMS before publishing", async () => {
  const [checks, release] = await Promise.all(
    ["checks.yml", "release.yml"].map(workflow),
  );
  const desktop = checks.jobs.desktop;
  assert.equal(desktop.environment, "${{ inputs.release && 'release' || '' }}");
  const secretSteps = Object.values(checks.jobs)
    .flatMap((job) => job.steps)
    .filter((step) => JSON.stringify(step).includes("secrets."));
  assert.ok(secretSteps.length);
  for (const step of secretSteps)
    for (const [name, value] of Object.entries(step.env ?? {}))
      assert.equal(
        value,
        `\${{ inputs.release && secrets.${name} || '' }}`,
        `${name} must be empty outside releases`,
      );
  const steps = release.jobs.release.steps;
  const index = (predicate) => steps.findIndex(predicate);
  const sign = index(
    (step) => step.run === "bun scripts/sign-release.ts release",
  );
  const attest = index((step) =>
    step.uses?.startsWith("actions/attest-build-provenance@"),
  );
  const publish = index((step) => step.run?.includes("gh release create"));
  assert.ok(sign >= 0 && sign < attest && attest < publish);
  assert.equal(release.jobs.release.environment, "release");
});
