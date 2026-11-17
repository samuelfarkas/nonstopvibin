import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import YAML from "yaml";
import { ConfigWriter } from "../src/server/config.ts";
import { coreConfigPath } from "../src/server/data-directory.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";

test("config writes return the saved snapshot, serialize, and recover after failure", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-config-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Fixture");
    assert.equal(profile.strategy, "fill-first");
    assert.equal(profile.sessionAffinity, true);
    const config = new ConfigWriter(store);
    const path = coreConfigPath(directory, profile.id);
    const first = await config.write(profile.id, 1234);
    assert.deepEqual(YAML.parse(await readFile(path, "utf8")), first);
    assert.equal(first.routing["session-affinity"], true);
    assert.equal((await stat(path)).mode & 0o777, 0o600);
    const inode = (await stat(path)).ino;

    store.saveProfile({
      ...profile,
      strategy: "round-robin",
      sessionAffinity: false,
    });
    const writes = await Promise.all([
      config.write(profile.id, 1235),
      config.write(profile.id, 1236),
    ]);
    assert.deepEqual(
      writes.map((config) => config.port),
      [1235, 1236],
    );
    assert.deepEqual(YAML.parse(await readFile(path, "utf8")), writes[1]);
    assert.equal(
      (await stat(path)).ino,
      inode,
      "preserve the core's file watch",
    );
    assert.equal(writes[1].routing["session-affinity"], false);
    assert.equal(first.routing.strategy, "fill-first");
    assert.equal(writes[1].routing.strategy, "round-robin");

    await rm(path);
    await mkdir(path);
    await assert.rejects(config.write(profile.id, 1237));
    await rm(path, { recursive: true });
    const recovered = await config.write(profile.id, 1238);
    assert.equal(recovered.port, 1238);
    assert.deepEqual(YAML.parse(await readFile(path, "utf8")), recovered);
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("a settings change saved while the config's directories are prepared is in the written config", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-config-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Fixture");
    const writing = new ConfigWriter(store).write(profile.id, 1234);
    // Only microtasks pass here, so the write is started but none of its filesystem calls has finished.
    for (let turn = 0; turn < 10; turn++) await Promise.resolve();
    store.saveProfile({ ...profile, strategy: "round-robin" });
    assert.equal((await writing).routing.strategy, "round-robin");
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});
