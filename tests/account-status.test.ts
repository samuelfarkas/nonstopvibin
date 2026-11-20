import test from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, stat, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { Json } from "../src/shared/types.ts";
import { Accounts } from "../src/server/accounts.ts";
import { credentialDirectory } from "../src/server/data-directory.ts";
import { temporaryStore } from "./support/application.ts";

test("dead refresh tokens show as Reconnect needed instead of a cooldown", async () => {
  const { directory, store, remove } = await temporaryStore(
    "nonstopvibin-status-",
  );
  try {
    const profile = store.createProfile("Fixture");
    const file = (name: string, fields: Record<string, Json>) => ({
      name,
      provider: "claude",
      account_type: "oauth",
      auth_index: name,
      ...fields,
    });
    // A running core that lists these files.
    const accounts = new Accounts(
      {
        usable: () => true,
        // No restarts: the core's record stays the same.
        runtimes: new Map(),
        management: async (_profileId, path) => {
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
        },
      },
      store,
    );
    await mkdir(credentialDirectory(directory, profile.id), {
      recursive: true,
    });
    await accounts.sync(profile.id);
    assert.deepEqual(
      accounts.list(profile.id).map((account) => [account.id, account.status]),
      [
        ["invalid-grant.json", "Reconnect needed"],
        ["unauthorized.json", "Reconnect needed"],
        ["expired.json", "Needs attention"],
        ["cooldown.json", "Cooling down"],
        ["ready.json", "Ready"],
      ],
    );
  } finally {
    await remove();
  }
});

test("account sync re-reads a Claude identity only when its file changes and repairs only loose modes", async () => {
  const { directory, store, remove } = await temporaryStore(
    "nonstopvibin-identity-",
  );
  try {
    const profile = store.createProfile("Fixture");
    // A running core that lists this file.
    const accounts = new Accounts(
      {
        usable: () => true,
        // No restarts: the core's record stays the same.
        runtimes: new Map(),
        management: async () => ({
          files: [
            { name: "seat.json", provider: "claude", auth_index: "seat" },
          ],
        }),
      },
      store,
    );
    const auth = credentialDirectory(directory, profile.id);
    await mkdir(auth, { recursive: true });
    const path = join(auth, "seat.json");
    const write = (organization: string) =>
      writeFile(
        path,
        JSON.stringify({ type: "claude", organization_name: organization }),
        { mode: 0o644 },
      );
    const organization = async () => {
      await accounts.sync(profile.id);
      return accounts.list(profile.id)[0]?.organizationName;
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
    await remove();
  }
});
