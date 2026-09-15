import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import YAML from "yaml";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";

test("config writes return the saved snapshot, serialize, and recover after failure", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-config-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Fixture", "forest");
    assert.equal(profile.strategy, "fill-first");
    assert.equal(profile.sessionAffinity, true);
    const core = new CorePool(store, "unused");
    const path = join(core.directory(profile.id), "config.yaml");
    const first = await core.writeConfig(profile.id, 1234);
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
      core.writeConfig(profile.id, 1235),
      core.writeConfig(profile.id, 1236),
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
    await assert.rejects(core.writeConfig(profile.id, 1237));
    await rm(path, { recursive: true });
    const recovered = await core.writeConfig(profile.id, 1238);
    assert.equal(recovered.port, 1238);
    assert.deepEqual(YAML.parse(await readFile(path, "utf8")), recovered);
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});
