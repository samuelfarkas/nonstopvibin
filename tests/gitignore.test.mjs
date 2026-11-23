import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

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
