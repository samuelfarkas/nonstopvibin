import { test } from "node:test";
import assert from "node:assert/strict";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import {
  agentConnectionsDirectory,
  coreArguments,
  coreConfigPath,
  coreLedgerDirectory,
  credentialDirectory,
  dataDirectory,
  databasePath,
  oauthPendingDirectory,
  profileDirectory,
  serverLockPath,
  sessionPath,
  vaultKeyPath,
} from "../src/server/data-directory.ts";

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

test("the data directory layout keeps the names installed apps already use", () => {
  const data = "/data";
  const id = "00000000-0000-4000-8000-000000000000";
  assert.equal(databasePath(data), "/data/nonstopvibin.sqlite");
  assert.equal(vaultKeyPath(data), "/data/vault.key");
  assert.equal(profileDirectory(data, id), `/data/profiles/${id}`);
  assert.equal(credentialDirectory(data, id), `/data/profiles/${id}/auth`);
  assert.equal(coreConfigPath(data, id), `/data/profiles/${id}/config.yaml`);
  assert.deepEqual(coreArguments(data, id), [
    "--config",
    `/data/profiles/${id}/config.yaml`,
  ]);
  assert.equal(oauthPendingDirectory(data), "/data/oauth-pending");
  assert.equal(coreLedgerDirectory(data), "/data/core-processes");
  assert.equal(agentConnectionsDirectory(data), "/data/agent-connections");
  assert.equal(sessionPath(data), "/data/server.json");
  assert.equal(serverLockPath(data), "/data/server.lock.sqlite");
});
