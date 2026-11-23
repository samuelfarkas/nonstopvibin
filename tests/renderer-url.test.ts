import test from "node:test";
import assert from "node:assert/strict";
import samples from "../src/shared/generated/domain/renderer-urls.json";
import { openedWith } from "../src/shared/renderer-url.ts";

test("the renderer reads back each link Rust opens it with", () => {
  assert.ok(samples.length > 0);
  for (const { url, opened } of samples)
    assert.deepEqual(openedWith(new URL(url)), opened, url);
});
