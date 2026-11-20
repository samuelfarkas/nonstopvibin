// Run with `bun tests/tray-guard.cjs` (isolated Electron renderer).
// A failed /state poll must not install the close guard in the tray view.
const assert = require("node:assert/strict");
const { writeFileSync } = require("node:fs");
const { join, resolve } = require("node:path");
const electron = require("electron");
const { checkPage, launchPage } = require("./support/electron-page.cjs");

if (typeof electron === "string") {
  launchPage("tray-guard", __filename, (directory) => {
    require("esbuild").buildSync({
      // The real entry: it picks the main or the tray root from the URL.
      entryPoints: [resolve(__dirname, "../src/main.tsx")],
      bundle: true,
      outfile: join(directory, "app.js"),
      loader: { ".css": "empty" },
      define: {
        "process.env.NODE_ENV": '"production"',
        "import.meta.hot": "undefined",
      },
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
      window.nonstopvibin = { bootstrap: async () => ({ token: 't' }), releaseWindow: async () => {} };
      window.fetch = async (url) => {
        if (!String(url).endsWith('/api/state')) return new Response('{}', { status: 404 });
        return ++stateCalls === 1
          ? new Response(${JSON.stringify(JSON.stringify(state))}, { status: 200 })
          : new Response(JSON.stringify({ error: { message: 'Fixture poll failed' } }), { status: 503 });
      };
    </script><script src="app.js"></script>`,
    );
  });
} else {
  checkPage(
    "Tray guard check",
    { show: false },
    15000,
    async (window, directory) => {
      const guardsAfterFailedPolls = async (query) => {
        await window.loadFile(join(directory, "index.html"), { query });
        return window.webContents.executeJavaScript(`(async () => {
          while (stateCalls < 4) await new Promise(r => setTimeout(r, 20));
          await new Promise(r => setTimeout(r, 50));
          return guards;
        })()`);
      };
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
    },
  );
}
