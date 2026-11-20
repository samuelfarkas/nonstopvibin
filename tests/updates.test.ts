import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { updateChecker } from "../src/desktop/updates.ts";
import {
  followLinuxLoginItem,
  linuxLoginItem,
  setLinuxLoginItem,
} from "../src/desktop/autostart.ts";

test("AppImage login entries can follow a renamed update without enabling opted-out startup", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-update-login-"));
  try {
    await followLinuxLoginItem(directory, "/apps/NonstopVibin-0.1.1.AppImage");
    assert.equal(linuxLoginItem(directory), false, "opted out stays out");
    await setLinuxLoginItem(
      directory,
      "/apps/NonstopVibin-0.1.1.AppImage",
      true,
    );
    await followLinuxLoginItem(directory, "/apps/NonstopVibin-0.1.2.AppImage");
    const entry = await readFile(
      join(directory, "autostart/app.nonstopvibin.desktop"),
      "utf8",
    );
    assert.match(entry, /NonstopVibin-0\.1\.2\.AppImage/);
    assert.doesNotMatch(entry, /0\.1\.1/);
    await setLinuxLoginItem(
      directory,
      "/apps/NonstopVibin-0.1.2.AppImage",
      false,
    );
    assert.equal(linuxLoginItem(directory), false);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("updates serialize downloads, defer restart, and stop checking once downloaded", async () => {
  const calls: string[] = [];
  let response = 1;
  let quitting = false;
  const check = updateChecker(
    {
      checkForUpdates: async () => {
        calls.push("check");
        const updateInfo = {
          version: "0.1.2",
          files: [],
          releaseDate: "2026-09-14",
          path: "app.zip",
          sha512: "fixture",
        };
        return { isUpdateAvailable: true, updateInfo, versionInfo: updateInfo };
      },
      downloadUpdate: async () => {
        calls.push("download");
        return [];
      },
      quitAndInstall: () => {
        calls.push("install");
      },
    },
    {
      showMessageBox: async () => {
        calls.push("prompt");
        return { response, checkboxChecked: false };
      },
    },
    async (install) => {
      calls.push("shutdown");
      install();
    },
    () => quitting,
  );
  await Promise.all([check(), check(true)]);
  assert.deepEqual(calls, ["check", "download", "prompt"]);
  await check();
  assert.equal(calls.length, 3);
  response = 0;
  await check(true);
  assert.deepEqual(calls.slice(3), ["prompt", "shutdown", "install"]);
  quitting = true;
  await check(true);
  assert.equal(calls.length, 6);
});

test("failed checks can retry; shutdown during a download never prompts or installs", async () => {
  let attempts = 0;
  let prompts = 0;
  let quitting = false;
  const check = updateChecker(
    {
      checkForUpdates: async () => {
        if (++attempts === 1) throw new Error("offline");
        const updateInfo = {
          version: "0.1.2",
          files: [],
          releaseDate: "2026-09-14",
          path: "app.zip",
          sha512: "fixture",
        };
        return { isUpdateAvailable: true, updateInfo, versionInfo: updateInfo };
      },
      downloadUpdate: async () => {
        quitting = true;
        return [];
      },
      quitAndInstall: () => {
        throw new Error("must not install");
      },
    },
    {
      showMessageBox: async () => {
        prompts++;
        return { response: 0, checkboxChecked: false };
      },
    },
    async () => {
      throw new Error("must not restart");
    },
    () => quitting,
  );
  await check();
  await check();
  assert.equal(attempts, 2);
  assert.equal(prompts, 0);
});
