import { test } from "node:test";
import assert from "node:assert/strict";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { dataDirectory } from "../src/server/data-directory.ts";

test("desktop and standalone default to the home folder and allow isolated overrides", () => {
  assert.equal(dataDirectory(""), join(homedir(), ".nonstopvibin"));
  assert.equal(dataDirectory(".test-runtime"), resolve(".test-runtime"));
  const previous = process.env.NONSTOPVIBIN_DATA_DIR;
  process.env.NONSTOPVIBIN_DATA_DIR = ".env-runtime";
  try {
    assert.equal(dataDirectory(), resolve(".env-runtime"));
  } finally {
    if (previous === undefined) delete process.env.NONSTOPVIBIN_DATA_DIR;
    else process.env.NONSTOPVIBIN_DATA_DIR = previous;
  }
});
