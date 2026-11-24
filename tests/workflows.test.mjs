import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import YAML from "yaml";

const project = resolve(import.meta.dirname, "..");

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
