// Run with `bun tests/page-layout.cjs`; measures shared page geometry in Electron.
const assert = require("node:assert/strict");
const { mkdtempSync, readFileSync, writeFileSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join, resolve } = require("node:path");
const electron = require("electron");

if (typeof electron === "string") {
  const directory = mkdtempSync(join(tmpdir(), "nv-layout-"));
  const css = ["styles", "subscriptions", "connect", "settings", "activity"]
    .map((name) =>
      readFileSync(resolve(__dirname, `../src/client/${name}.css`), "utf8"),
    )
    .join("\n");
  writeFileSync(
    join(directory, "index.html"),
    `<style>${css}</style><div id="root"></div>`,
  );
  const child = require("node:child_process").spawn(
    electron,
    [__filename, directory],
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
    const window = new BrowserWindow({
      show: true,
      webPreferences: { backgroundThrottling: false },
    });
    const timeout = setTimeout(() => {
      console.error("Page layout check timed out");
      app.exit(1);
    }, 30000);
    try {
      await window.loadFile(join(process.argv[2], "index.html"));
      for (const width of [360, 620, 850, 1280, 1800, 2400]) {
        window.setContentSize(width, 800);
        const samples = await window.webContents.executeJavaScript(`(() => {
          const samples = [];
          for (const page of ['subscriptions', 'activity', 'connect', 'settings']) {
            for (const tall of [false, true]) {
              const header = '<header class="page-header"><div class="page-title"><h1>Personal</h1><code>2 subscriptions · fill first</code></div>' +
                (page === 'subscriptions' ? '<div class="page-actions"><button class="button">Refresh</button><button class="button primary">Add subscription</button></div>' : '') + '</header>';
              const contentClass = {subscriptions: 'accounts', activity: 'activity-page', connect: 'connect-page', settings: 'settings-content'}[page];
              const body = '<div class="page-body"><div class="' + contentClass + '" style="height:' + (tall ? 1400 : 100) + 'px">Content</div></div>';
              document.getElementById('root').innerHTML = '<div class="app desktop"><aside class="sidebar"></aside><main class="main">' +
                header + body + '</main></div>';
              const rect = selector => {
                const {x,y,width,height} = document.querySelector(selector).getBoundingClientRect();
                return {x,y,width,height};
              };
              const content = rect('.' + contentClass);
              samples.push({page, tall, header: rect('.page-header'), title: rect('.page-title'), content,
                overflow: document.documentElement.scrollWidth > document.documentElement.clientWidth});
            }
          }
          return samples;
        })()`);
        const baseline = samples[0];
        for (const sample of samples) {
          const context = `${width}px ${sample.page} tall=${sample.tall}`;
          assert.deepEqual(
            sample.header,
            baseline.header,
            `${context}: header moved`,
          );
          assert.equal(
            sample.title.x,
            baseline.title.x,
            `${context}: title shifted sideways`,
          );
          assert.equal(
            sample.title.y,
            baseline.title.y,
            `${context}: title shifted vertically`,
          );
          assert.equal(
            sample.content.x,
            baseline.content.x,
            `${context}: content shifted`,
          );
          assert.equal(
            sample.content.y,
            baseline.content.y,
            `${context}: content shifted vertically`,
          );
          assert.equal(
            sample.content.width,
            baseline.content.width,
            `${context}: content resized`,
          );
          assert.equal(
            sample.content.x,
            sample.title.x,
            `${context}: content/header misaligned`,
          );
          assert.equal(
            sample.overflow,
            false,
            `${context}: horizontal overflow`,
          );
        }
      }
      console.log(
        "Page layout passed: four screens, six widths, with and without vertical overflow.",
      );
    } catch (error) {
      console.error(error);
      process.exitCode = 1;
    } finally {
      clearTimeout(timeout);
      app.exit(process.exitCode ?? 0);
    }
  });
}
