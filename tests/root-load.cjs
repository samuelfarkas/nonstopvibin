// Run with `bun tests/root-load.cjs` after `bun run build` (isolated Electron renderer).
// The built entry loads the main or tray root as a separate chunk; when that request fails
// the window shows the recovery view instead of staying blank.
const assert = require("node:assert/strict");
const { createReadStream, existsSync } = require("node:fs");
const { createServer } = require("node:http");
const { extname, join, resolve } = require("node:path");
const electron = require("electron");
const { checkPage, launchPage } = require("./support/electron-page.cjs");

if (typeof electron === "string") {
  launchPage("root-load", __filename, () => {});
} else {
  checkPage("Root load check", { show: false }, 15000, async (window) => {
    const client = resolve(__dirname, "../dist/client");
    const types = { ".js": "text/javascript", ".css": "text/css" };
    const failed = [];
    const server = createServer((request, response) => {
      const path = new URL(request.url, "http://localhost").pathname;
      const file = join(client, path === "/" ? "index.html" : path);
      if (/^\/assets\/(App|Tray)-[^/]+\.js$/.test(path) || !existsSync(file)) {
        failed.push(path);
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, {
        "Content-Type": types[extname(file)] ?? "text/html",
      });
      createReadStream(file).pipe(response);
    });
    await new Promise((done) => server.listen(0, "127.0.0.1", done));
    const uncaught = [];
    window.webContents.on("console-message", (event) => {
      if (event.message.includes("Uncaught")) uncaught.push(event.message);
    });
    try {
      for (const query of ["", "?view=tray"]) {
        failed.length = 0;
        await window.loadURL(
          `http://127.0.0.1:${server.address().port}/${query}`,
        );
        const text = await window.webContents.executeJavaScript(`(async () => {
          for (let i = 0; i < 100 && !document.querySelector('.fatal button'); i++)
            await new Promise(r => setTimeout(r, 20));
          return document.querySelector('.fatal')?.textContent ?? '';
        })()`);
        assert.ok(
          failed.some((path) => /\/(App|Tray)-/.test(path)),
          `the ${query || "main"} root chunk request was failed`,
        );
        assert.match(
          text,
          /Reload nonstopvibin/,
          `the ${query || "main"} window shows the recovery view`,
        );
      }
      assert.deepEqual(uncaught, [], "no unhandled rejection");
      console.log("Root load passed: a failed root chunk shows recovery.");
    } finally {
      server.close();
    }
  });
}
