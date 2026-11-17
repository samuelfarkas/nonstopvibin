import test from "node:test";
import assert from "node:assert/strict";
import type { ChildProcess } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  rm,
  stat,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Json } from "../src/shared/types.ts";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";

test("dead refresh tokens show as Reconnect needed instead of a cooldown", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-status-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const originalManagement = CorePool.prototype.management;
  try {
    const profile = store.createProfile("Fixture");
    const core = new CorePool(store, "unused");
    await mkdir(join(core.directory(profile.id), "auth"), { recursive: true });
    // SAFETY: syncAccounts only reads runtime state and account maps; the
    // child process is never touched, so an empty stub is enough.
    core.runtimes.set(profile.id, {
      child: {} as ChildProcess,
      port: 0,
      state: "running",
      stopping: false,
      accounts: [],
      chatgptAccountIds: new Map(),
      nextUsageAt: 0,
      usageActiveUntil: 0,
    });
    const file = (name: string, fields: Record<string, Json>) => ({
      name,
      provider: "claude",
      account_type: "oauth",
      auth_index: name,
      ...fields,
    });
    CorePool.prototype.management = async (_profileId, path) => {
      assert.equal(path, "/auth-files");
      return {
        files: [
          file("invalid-grant.json", {
            status: "error",
            status_message: "invalid grant (retrying)",
            unavailable: true,
          }),
          file("unauthorized.json", {
            status: "error",
            status_message: "unauthorized",
            unavailable: true,
          }),
          file("expired.json", {
            status: "error",
            status_message: "token expired",
            unavailable: true,
          }),
          file("cooldown.json", { unavailable: true }),
          file("ready.json", {}),
        ],
      };
    };
    await core.syncAccounts(profile.id);
    assert.deepEqual(
      core.accounts(profile.id).map((account) => [account.id, account.status]),
      [
        ["invalid-grant.json", "Reconnect needed"],
        ["unauthorized.json", "Reconnect needed"],
        ["expired.json", "Needs attention"],
        ["cooldown.json", "Cooling down"],
        ["ready.json", "Ready"],
      ],
    );
  } finally {
    CorePool.prototype.management = originalManagement;
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("account sync re-reads a Claude identity only when its file changes and repairs only loose modes", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-identity-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const originalManagement = CorePool.prototype.management;
  try {
    const profile = store.createProfile("Fixture");
    const core = new CorePool(store, "unused");
    const auth = join(core.directory(profile.id), "auth");
    await mkdir(auth, { recursive: true });
    // SAFETY: as above, syncAccounts never touches the child process.
    core.runtimes.set(profile.id, {
      child: {} as ChildProcess,
      port: 0,
      state: "running",
      stopping: false,
      accounts: [],
      chatgptAccountIds: new Map(),
      nextUsageAt: 0,
      usageActiveUntil: 0,
    });
    CorePool.prototype.management = async () => ({
      files: [{ name: "seat.json", provider: "claude", auth_index: "seat" }],
    });
    const path = join(auth, "seat.json");
    const write = (organization: string) =>
      writeFile(
        path,
        JSON.stringify({ type: "claude", organization_name: organization }),
        { mode: 0o644 },
      );
    const organization = async () => {
      await core.syncAccounts(profile.id);
      return core.accounts(profile.id)[0]?.organizationName;
    };
    await write("Org One");
    await chmod(path, 0o644);
    await utimes(path, 1_000_000, 1_000_000);
    assert.equal(await organization(), "Org One");
    assert.equal((await stat(path)).mode & 0o777, 0o600);
    // Same size and timestamp: the cached parse stands, so the file was not re-read.
    await write("Org Two");
    await utimes(path, 1_000_000, 1_000_000);
    assert.equal(await organization(), "Org One");
    await utimes(path, 1_000_000, 1_000_001);
    assert.equal(await organization(), "Org Two");
  } finally {
    CorePool.prototype.management = originalManagement;
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});
