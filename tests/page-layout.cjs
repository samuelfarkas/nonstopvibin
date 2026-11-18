// Run with `bun tests/page-layout.cjs` after `bun run build`: the built app, served by a real
// server with one running profile, keeps page geometry identical across pages and widths.
const assert = require("node:assert/strict");
const { mkdtempSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join, resolve } = require("node:path");
const { setTimeout: delay } = require("node:timers/promises");
const electron = require("electron");

if (typeof electron === "string") {
  const directory = mkdtempSync(join(tmpdir(), "nv-layout-"));
  (async () => {
    const { Application } = require("../src/server/server.ts");
    const { sessionUrl } = require("../src/shared/renderer-url.ts");
    const {
      installFakeCore,
      listeningCore,
    } = require("./support/fake-core.ts");
    const binary = await installFakeCore(
      join(directory, "core"),
      listeningCore(
        `(_req, res) => res.end(JSON.stringify({ data: [{ id: "fixture-model" }] }))`,
      ),
    );
    const app = await Application.create({
      directory: join(directory, "data"),
      agentHome: join(directory, "agent-home"),
      binary,
      clientDirectory: resolve(__dirname, "../dist/client"),
      port: 0,
    });
    let code = 1;
    try {
      const profile = app.store.createProfile("Personal");
      app.store.saveApiAccount(
        profile.id,
        {
          id: "layout-fixture",
          name: "Layout fixture",
          provider: "custom",
          baseUrl: "https://api.example.invalid/v1",
          prefix: "",
          disabled: false,
          models: [{ id: "fixture-model", protocol: "openai" }],
        },
        "synthetic-key",
      );
      await app.cores.pool.start(profile.id);
      const child = require("node:child_process").spawn(
        electron,
        [
          __filename,
          join(directory, "user"),
          sessionUrl(app.origin, app.token),
        ],
        { stdio: "inherit" },
      );
      code = await new Promise((done) =>
        child.on("exit", (exit) => done(exit ?? 1)),
      );
    } finally {
      await app.close();
      rmSync(directory, { recursive: true, force: true });
      rmSync(app.agentSetup.socketDirectory, { recursive: true, force: true });
    }
    process.exitCode = code;
  })().catch((error) => {
    console.error(error);
    rmSync(directory, { recursive: true, force: true });
    process.exitCode = 1;
  });
} else {
  const { app, BrowserWindow } = electron;
  const [userData, url] = process.argv.slice(2);
  app.setPath("userData", userData);
  app.whenReady().then(async () => {
    // Hidden, and allowed past the display's size, so no window manager clamps the widths.
    const window = new BrowserWindow({
      show: false,
      enableLargerThanScreen: true,
      webPreferences: { backgroundThrottling: false },
    });
    const timeout = setTimeout(() => {
      console.error("Page layout check timed out");
      app.exit(1);
    }, 60000);
    const run = (code) => window.webContents.executeJavaScript(code);
    const until = async (code) => {
      while (!(await run(code))) await delay(20);
    };
    // Each page and a block only its rendered content has; geometry is the page body's first block.
    const pages = [
      ["Subscriptions", ".accounts"],
      ["Activity", ".activity-overview"],
      ["Connect agents", ".connect-page .agent-tabs"],
      ["Settings", "#usage-retention"],
    ];
    try {
      await window.loadURL(url);
      await until("Boolean(document.querySelector('.main-nav'))");
      for (const width of [360, 620, 850, 1280, 1800, 2400]) {
        const samples = [];
        // Short enough that every page scrolls, and tall enough that none does.
        for (const height of [200, 3000]) {
          window.setContentSize(width, height);
          await until(`innerWidth === ${width} && innerHeight === ${height}`);
          for (const [title, ready] of pages) {
            await run(
              `document.querySelector('[title=${JSON.stringify(title)}]').click()`,
            );
            await until(
              `Boolean(document.querySelector('.page-body ${ready}'))`,
            );
            samples.push({
              page: title,
              height,
              ...(await run(`(() => {
                const rect = selector => {
                  const {x, y, width, height} = document.querySelector(selector).getBoundingClientRect();
                  return {x, y, width, height};
                };
                const page = document.documentElement;
                return {
                  header: rect('.page-header'),
                  title: rect('.page-title'),
                  content: rect('.page-body > :first-child'),
                  scrolls: page.scrollHeight > page.clientHeight,
                  overflow: page.scrollWidth > page.clientWidth,
                };
              })()`)),
            });
          }
        }
        const baseline = samples[0];
        for (const sample of samples) {
          const context = `${width}px ${sample.page} at ${sample.height}px`;
          assert.equal(
            sample.scrolls,
            sample.height === 200,
            `${context}: vertical overflow is not what the check needs`,
          );
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
        "Page layout passed: four rendered pages, six widths, with and without vertical overflow.",
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
