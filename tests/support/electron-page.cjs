// The two halves the renderer checks share (tests/focus.cjs, logo-animation.cjs, tray-guard.cjs):
// a page written to a fresh directory, and an Electron run that checks it in one window.
const { mkdtempSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");

/** Outside Electron: `write(directory)` fills a fresh directory, then `script` checks it in Electron. */
function launchPage(name, script, write) {
  const directory = mkdtempSync(join(tmpdir(), `nv-${name}-`));
  write(directory);
  const child = require("node:child_process").spawn(
    require("electron"),
    [script, directory, "--headless"],
    { stdio: "inherit" },
  );
  child.on("exit", (code) => {
    rmSync(directory, { recursive: true, force: true });
    process.exitCode = code ?? 1;
  });
}

/**
 * Inside Electron: opens one window with `options`, runs `check(window, directory)` and exits
 * with its result; a check still running after `limit` ms fails with `label`.
 */
function checkPage(label, options, limit, check) {
  const { app, BrowserWindow } = require("electron");
  const directory = process.argv[2];
  app.setPath("userData", directory);
  app.whenReady().then(async () => {
    const window = new BrowserWindow(options);
    const timeout = setTimeout(() => {
      console.error(`${label} timed out`);
      app.exit(1);
    }, limit);
    try {
      await check(window, directory);
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

module.exports = { checkPage, launchPage };
