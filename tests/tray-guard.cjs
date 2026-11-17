// Run with `bun tests/tray-guard.cjs` (isolated Electron renderer).
// A failed /state poll must not install the close guard in the tray view.
const assert = require("node:assert/strict");
const { mkdtempSync, writeFileSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join, resolve } = require("node:path");
const electron = require("electron");

if (typeof electron === "string") {
  const directory = mkdtempSync(join(tmpdir(), "nv-tray-guard-"));
  require("esbuild").buildSync({
    stdin: {
      contents: `import React from 'react';
        import {createRoot} from 'react-dom/client';
        import {App} from './src/client/App.tsx';
        createRoot(document.getElementById('root')).render(<App />);`,
      resolveDir: resolve(__dirname, ".."),
      loader: "tsx",
    },
    bundle: true,
    outfile: join(directory, "app.js"),
    loader: { ".css": "empty" },
    define: { "process.env.NODE_ENV": '"production"' },
  });
  const state = {
    profiles: [
      {
        id: "p",
        slug: "work",
        name: "Work",
        strategy: "round-robin",
        sessionAffinity: false,
        enabled: true,
        createdAt: "",
        runtime: "running",
        accounts: [],
        endpoint: "",
      },
    ],
    coreVersion: "",
    gateway: "",
    usageRetentionDays: 0,
    version: "",
    desktop: true,
    errors: [],
  };
  writeFileSync(
    join(directory, "index.html"),
    `<div id="root"></div><script>
      // Count live beforeunload listeners; poll quickly; first /state succeeds, later polls fail.
      window.guards = 0;
      window.stateCalls = 0;
      const add = window.addEventListener, remove = window.removeEventListener;
      window.addEventListener = function (type, ...rest) { if (type === 'beforeunload') guards++; return add.call(this, type, ...rest); };
      window.removeEventListener = function (type, ...rest) { if (type === 'beforeunload') guards--; return remove.call(this, type, ...rest); };
      const interval = window.setInterval;
      window.setInterval = (callback, ms, ...rest) => interval(callback, Math.min(ms, 50), ...rest);
      window.nonstopvibin = { bootstrap: async () => ({ token: 't', desktop: true }), releaseWindow: async () => {} };
      window.fetch = async (url) => {
        if (!String(url).endsWith('/api/state')) return new Response('{}', { status: 404 });
        return ++stateCalls === 1
          ? new Response(${JSON.stringify(JSON.stringify(state))}, { status: 200 })
          : new Response(JSON.stringify({ error: { message: 'Fixture poll failed' } }), { status: 503 });
      };
    </script><script src="app.js"></script>`,
  );
  const child = require("node:child_process").spawn(
    electron,
    [__filename, directory, "--headless"],
    { stdio: "inherit" },
  );
  child.on("exit", (code) => {
    rmSync(directory, { recursive: true, force: true });
    process.exitCode = code ?? 1;
  });
} else {
  const { app, BrowserWindow } = electron;
  app.setPath("userData", process.argv[2]);
  app.whenReady().then(async () => {
    const window = new BrowserWindow({ show: false });
    const timeout = setTimeout(() => {
      console.error("Tray guard check timed out");
      app.exit(1);
    }, 15000);
    const guardsAfterFailedPolls = async (query) => {
      await window.loadFile(join(process.argv[2], "index.html"), { query });
      return window.webContents.executeJavaScript(`(async () => {
        while (stateCalls < 4) await new Promise(r => setTimeout(r, 20));
        await new Promise(r => setTimeout(r, 50));
        return guards;
      })()`);
    };
    try {
      assert.equal(
        await guardsAfterFailedPolls({}),
        1,
        "control: the main window keeps a visible poll failure as a draft",
      );
      assert.equal(
        await guardsAfterFailedPolls({ view: "tray" }),
        0,
        "the tray installs no beforeunload guard when a poll fails",
      );
      console.log("Tray guard passed: failed polls leave the tray closable.");
    } catch (error) {
      console.error(error);
      process.exitCode = 1;
    } finally {
      clearTimeout(timeout);
      window.destroy();
      app.exit(process.exitCode || 0);
    }
  });
}
