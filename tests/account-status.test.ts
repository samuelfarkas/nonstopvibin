import test from "node:test";
import assert from "node:assert/strict";
import type { ChildProcess } from "node:child_process";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
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
    const profile = store.createProfile("Fixture", "forest");
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
