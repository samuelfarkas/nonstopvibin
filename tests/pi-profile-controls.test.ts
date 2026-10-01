import assert from "node:assert/strict";
import { test } from "node:test";
import {
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { runInNewContext } from "node:vm";
import { piProfileControls } from "../src/server/pi-profile-controls.ts";

test("pi profile controls preserve session routing across selection, restore, failures and concurrent sessions", async () => {
  const fixture = await readFile(
    new URL("./fixtures/pi-profile-controls.mjs", import.meta.url),
    "utf8",
  );
  const directory = await realpath(
    await mkdtemp(join(tmpdir(), "nv-pi-controls-")),
  );
  try {
    const source = join(directory, "controls.mjs");
    await writeFile(
      source,
      `${piProfileControls}\nexport { connectProfileControls };`,
    );
    const { connectProfileControls } = await import(pathToFileURL(source).href);
    const result: Promise<void> = runInNewContext(
      `const verify = ${fixture}\nverify(assert, connectProfileControls, directory, helpers)`,
      {
        assert,
        connectProfileControls,
        directory,
        helpers: { createHash, mkdir, writeFile },
      },
    );
    await result;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
