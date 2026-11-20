import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { spin } from "../scripts/mirrorball.mjs";

test("the logo animation steps through the generated spin sprite's frames in its duration", async () => {
  const css = await readFile(
    new URL("../src/client/styles.css", import.meta.url),
    "utf8",
  );
  assert.ok(
    css.includes(
      `url("/mirrorball-spin.png") 0 0 / ${spin.frames * 100}% 100% no-repeat`,
    ),
  );
  assert.ok(
    css.includes(
      `animation: nv-spin ${spin.durationMs}ms steps(${spin.frames - 1});`,
    ),
  );
});
