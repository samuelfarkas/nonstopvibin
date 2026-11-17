// Run with `bun run test:desktop`; the child uses real Electron with isolated data.
const assert = require("node:assert/strict");
const {
  readFileSync,
  writeFileSync,
  rmSync,
  realpathSync,
} = require("node:fs");
const { join, resolve } = require("node:path");
const { setTimeout: delay } = require("node:timers/promises");
const electron = require("electron");
const harness = require("./desktop-harness.cjs");

if (typeof electron === "string") {
  // The first phase takes about 20 s on the macOS runner and the child's own checks give up
  // at 50 s; past 90 s it is stalled where only this limit can stop it.
  const limit = 90_000;
  rmSync(harness.artifactRoot, { recursive: true, force: true });
  (async () => {
    const run = harness.launch("smoke", __filename, "inherit");
    const cancel = harness.limit(limit, "Desktop smoke", () => run);
    const { code, error } = await run.exited;
    cancel();
    if (run.expired) return;
    try {
      if (error) throw error;
      let result;
      try {
        result = JSON.parse(
          readFileSync(join(run.directory, "result.json"), "utf8"),
        );
      } catch {
        result = { error: "The Electron child wrote no result." };
      }
      assert.equal(result.ok, true, result.error);
      assert.equal(code, 0);
      console.log("Desktop lifecycle passed:", JSON.stringify(result.samples));
    } catch (error) {
      console.error(error);
      await harness.diagnose(run, "failure");
      await harness.terminate(run);
      process.exit(1);
    }
    await harness.terminate(run);
    // Then check that profile cores never outlive a signalled or killed app.
    require("./desktop-lifetime.cjs");
  })();
} else {
  const { stage, api } = harness.instrument("result.json");
  const { app, BrowserWindow, Menu } = electron;
  const directory = process.env.NONSTOPVIBIN_DATA_DIR;
  const samples = [];
  const monitors = [];
  const childProcess = require("node:child_process");
  const spawn = childProcess.spawn;
  childProcess.spawn = (...args) => {
    const child = spawn(...args);
    if (String(args[0]).endsWith("/tray-click-monitor")) monitors.push(child);
    return child;
  };
  app.setAppPath(resolve(__dirname, ".."));
  stage("load-main");
  require("../dist/desktop/main.cjs");
  void app.whenReady().then(() => stage("app-ready"));
  const deadline = Date.now() + 50_000;
  async function until(check) {
    while (!(await check())) {
      if (Date.now() > deadline) throw new Error("Desktop check timed out");
      await delay(50);
    }
  }
  // A call into a page never settles if the page goes away first; fail at once instead.
  function live(window, label, call) {
    if (window.isDestroyed())
      return Promise.reject(new Error(`${label}: the window is destroyed`));
    const contents = window.webContents;
    return new Promise((resolve, reject) => {
      const destroyed = () =>
        reject(
          new Error(`${label}: the window was destroyed before it answered`),
        );
      const gone = (_event, details) =>
        reject(new Error(`${label}: the renderer ended (${details.reason})`));
      contents.once("destroyed", destroyed);
      contents.once("render-process-gone", gone);
      call(contents)
        .then(resolve, reject)
        .finally(() => {
          contents.off("destroyed", destroyed);
          contents.off("render-process-gone", gone);
        });
    });
  }
  const run = (window, code) =>
    live(
      window,
      `Renderer call ${JSON.stringify(code.slice(0, 80))}`,
      (contents) => contents.executeJavaScript(code),
    );
  // Counts the page's beforeunload listeners. The attached debugger also keeps Chromium from
  // closing a page that takes over 500 ms to answer beforeunload, which a slow runner hits;
  // that heuristic is Chromium's, not the deferred close under test.
  const guards = (window) =>
    live(window, "Counting beforeunload guards", async (contents) => {
      if (!contents.debugger.isAttached()) contents.debugger.attach();
      const send = (method, params) =>
        contents.debugger.sendCommand(method, params);
      const { result } = await send("Runtime.evaluate", {
        expression: "window",
        objectGroup: "guards",
      });
      const { listeners } = await send("DOMDebugger.getEventListeners", {
        objectId: result.objectId,
      });
      await send("Runtime.releaseObjectGroup", { objectGroup: "guards" });
      return listeners.filter((listener) => listener.type === "beforeunload")
        .length;
    });
  // Closes the main window once `count` guards can veto it, then waits for the deferred close.
  async function closeGuarded(window, count, retained) {
    await until(async () => (await guards(window)) >= count);
    window.close();
    await until(() => {
      assert.equal(window.isDestroyed(), false, retained);
      return !window.isVisible();
    });
  }
  const viewMenu = () =>
    Menu.getApplicationMenu()?.items.find((item) => item.label === "View")
      ?.submenu;
  const click = (label) => {
    const item = viewMenu().items.find((entry) => entry.label === label);
    assert.ok(item, label);
    item.click();
  };
  let proxy;
  async function main() {
    assert.equal(app.getPath("userData"), directory);
    for (const name of ["sessionData", "crashDumps", "logs"])
      assert.equal(
        realpathSync(app.getPath(name)),
        realpathSync(join(directory, name)),
      );
    stage("wait-menu");
    await until(() =>
      viewMenu()?.items.some((item) => item.label === "Open main window"),
    );
    assert.equal(
      BrowserWindow.getAllWindows().length,
      0,
      "background startup creates no windows",
    );
    for (let cycle = 0; cycle < 3; cycle++) {
      const started = Date.now();
      stage(`c${cycle}:open-main`);
      click("Open main window");
      await until(() =>
        BrowserWindow.getAllWindows().some((window) => window.isVisible()),
      );
      const window = BrowserWindow.getAllWindows()[0];
      await until(() => !window.webContents.isLoading());
      stage(`c${cycle}:bootstrap`);
      const { token } = await run(window, "window.nonstopvibin.bootstrap()");
      const origin = new URL(window.webContents.getURL()).origin;
      if (cycle === 0) api(origin, token);
      const state = await fetch(`${origin}/api/state`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      assert.equal(
        state.status,
        200,
        "bootstrap accepts the recreated renderer",
      );
      samples.push({
        phase: "open",
        cycle,
        milliseconds: Date.now() - started,
      });
      if (cycle === 0) {
        stage("c0:create-profile");
        assert.equal(
          await run(window, "Notification.requestPermission()"),
          "denied",
          "permission requests are denied",
        );
        const request = async (path, method = "GET", body) => {
          const response = await fetch(`${origin}/api${path}`, {
            method,
            headers: {
              Authorization: `Bearer ${token}`,
              "Content-Type": "application/json",
            },
            body: body === undefined ? undefined : JSON.stringify(body),
          });
          assert.ok(response.ok, `Fixture request ${path}: ${response.status}`);
          return response.json();
        };
        const profile = await request("/profiles", "POST", {
          name: "Desktop fixture",
        });
        await until(() =>
          run(window, "Boolean(document.querySelector('.proxy-card button'))"),
        );
        await run(
          window,
          `{
          const original = window.fetch;
          window.fetch = (url, options) => {
            if (!String(url).endsWith('/start')) return original(url, options);
            return new Promise(resolve => {
              window.finishFixtureOperation = () => {
                window.fetch = original;
                resolve(new Response(JSON.stringify({error:{message:'Fixture operation failed'}}), {status:503}));
              };
            });
          };
          document.querySelector('.proxy-card button').click();
        }`,
        );
        stage("c0:background-failure");
        await closeGuarded(window, 1, "a pending operation defers the close");
        await run(window, "window.finishFixtureOperation()");
        await until(() =>
          run(
            window,
            "document.querySelector('.global-alert')?.textContent.includes('Fixture operation failed')",
          ),
        );
        await delay(100);
        assert.equal(
          window.isDestroyed(),
          false,
          "background failure remains available to read",
        );
        app.emit("activate");
        await until(() => window.isVisible());
        await run(
          window,
          "document.querySelector('[aria-label=\"Dismiss error\"]').click()",
        );
        await delay(100);
        assert.equal(
          window.isVisible(),
          true,
          "reopening cancels deferred close",
        );
        stage("c0:start-profile");
        await request(`/profiles/${profile.id}/start`, "POST");
        const { key } = await request(`/profiles/${profile.id}/key`);
        proxy = { endpoint: `${origin}/p/${profile.slug}/v1`, key };
        await until(() =>
          run(
            window,
            "Boolean(document.querySelector('[aria-label=\"New profile\"]'))",
          ),
        );
        // Exercise the real modal beforeunload guard, then discard through its close button.
        stage("c0:unsaved-form");
        await run(
          window,
          "document.querySelector('[aria-label=\"New profile\"]').click()",
        );
        await closeGuarded(window, 1, "unfinished form is retained");
        assert.equal(
          window.isDestroyed(),
          false,
          "unfinished form is retained",
        );
        app.emit("activate");
        await until(() => window.isVisible());
        await run(
          window,
          "document.querySelector('[aria-label=\"Close dialog\"]').click()",
        );
        await delay(100);
      }
      stage(`c${cycle}:logo`);
      await until(() =>
        run(window, "Boolean(document.querySelector('.nv-ball'))"),
      );
      assert.equal(
        await run(
          window,
          `(async () => {
          const logo = document.querySelector('.nv-logo');
          const animations = logo.getAnimations({subtree:true});
          if (animations.some(animation => animation.effect.getTiming().iterations === Infinity)) return -1;
          await Promise.all(animations.map(animation => animation.finished));
          return logo.getAnimations({subtree:true}).length;
        })()`,
        ),
        0,
        "logo finishes its finite animation and stays idle",
      );
      if (cycle === 1) {
        stage("c1:settings");
        // The start screen shows the logo too; the sidebar exists once state has loaded.
        await until(() =>
          run(
            window,
            "Boolean(document.querySelector('[title=\"Settings\"]'))",
          ),
        );
        await run(
          window,
          "document.querySelector('[title=\"Settings\"]').click()",
        );
        await until(() =>
          run(window, "Boolean(document.querySelector('#usage-retention'))"),
        );
        const retention = async (days) => {
          await run(
            window,
            `{
            const select = document.querySelector('#usage-retention');
            select.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
          }`,
          );
          await until(() =>
            run(window, "Boolean(document.querySelector('[role=\"option\"]'))"),
          );
          await run(
            window,
            `{
            const label = ${JSON.stringify({ 0: "Keep all history", 30: "30 days", 90: "90 days", 365: "1 year" })}[${days}];
            const option = [...document.querySelectorAll('[role="option"]')].find(item => item.textContent === label);
            option.focus();
            option.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
          }`,
          );
          await until(() =>
            run(
              window,
              "document.querySelector('#usage-retention').getAttribute('aria-expanded') === 'false'",
            ),
          );
        };
        await retention(30);
        const created = await fetch(`${origin}/api/profiles`, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${token}`,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({ name: "Settings fixture" }),
        });
        assert.equal(created.status, 201);
        const settingsProfile = await created.json();
        await until(() =>
          run(
            window,
            "Boolean(document.querySelector('[title=\"Settings fixture\"]'))",
          ),
        );
        const sidebarProfile = await run(
          window,
          "document.querySelector('.profiles [aria-current]').title",
        );
        await run(
          window,
          `{
          const picker = document.querySelector('#settings-profile');
          picker.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
        }`,
        );
        await until(() =>
          run(window, "Boolean(document.querySelector('[role=\"option\"]'))"),
        );
        await run(
          window,
          `{
          const option = [...document.querySelectorAll('[role="option"]')].find(item => item.textContent.includes('Settings fixture'));
          option.focus();
          option.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
        }`,
        );
        await until(() =>
          run(
            window,
            "document.querySelector('#profile-name')?.value === 'Settings fixture'",
          ),
        );
        assert.equal(
          await run(
            window,
            "document.querySelector('.profiles [aria-current]').title",
          ),
          sidebarProfile,
          "settings picker leaves sidebar profile unchanged",
        );
        assert.equal(
          await run(
            window,
            "document.querySelector('#usage-retention').textContent",
          ),
          "30 days",
          "switching settings profile preserves global draft",
        );
        await run(
          window,
          "document.querySelector('[aria-label=\"Keep sessions on the same account\"]').click()",
        );
        await until(async () => {
          const response = await fetch(`${origin}/api/state`, {
            headers: { Authorization: `Bearer ${token}` },
          });
          const current = await response.json();
          return (
            current.profiles.find(
              (profile) => profile.id === settingsProfile.id,
            ).sessionAffinity === !settingsProfile.sessionAffinity
          );
        });
        const current = await (
          await fetch(`${origin}/api/state`, {
            headers: { Authorization: `Bearer ${token}` },
          })
        ).json();
        assert.equal(
          current.profiles.find((profile) => profile.name === sidebarProfile)
            .sessionAffinity,
          true,
          "editing another profile leaves the original profile unchanged",
        );
        assert.equal(
          current.usageRetentionDays,
          0,
          "global retention draft was not applied",
        );
        console.log("Settings profile switching and isolated updates passed.");
        stage("c1:unsaved-settings");
        await closeGuarded(window, 1, "unsaved settings are retained");
        assert.equal(
          window.isDestroyed(),
          false,
          "unsaved settings are retained",
        );
        app.emit("activate");
        await until(() => window.isVisible());
        assert.equal(
          await run(
            window,
            "document.querySelector('#usage-retention').textContent",
          ),
          "30 days",
        );
        await retention(0);
        await delay(100);
        // One cleared guard must not discard another form in the same window.
        await retention(30);
        await run(
          window,
          "document.querySelector('[aria-label=\"New profile\"]').click()",
        );
        await closeGuarded(
          window,
          2,
          "settings and dialog drafts defer the close",
        );
        await retention(0);
        await delay(100);
        assert.equal(
          window.isDestroyed(),
          false,
          "remaining dialog keeps its draft",
        );
        await run(
          window,
          "document.querySelector('[aria-label=\"Close dialog\"]').click()",
        );
        await until(() => window.isDestroyed());
        samples.push({
          phase: "draft-finished-hidden",
          renderersReleased: true,
        });
      }
      if (cycle === 2) {
        stage("c2:settings-restored");
        await until(() =>
          run(window, "Boolean(document.querySelector('#usage-retention'))"),
        );
        assert.equal(
          await run(
            window,
            "document.querySelector('#usage-retention').textContent",
          ),
          "Keep all history",
          "settings page restores without applying the discarded limit",
        );
      }
      stage(`c${cycle}:close-main`);
      if (!window.isDestroyed()) window.close();
      await until(() => BrowserWindow.getAllWindows().length === 0);
      const response = await fetch(`${origin}/api/state`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      assert.equal(
        response.status,
        200,
        "backend remains available without a renderer",
      );
      const models = await fetch(`${proxy.endpoint}/models`, {
        headers: { Authorization: `Bearer ${proxy.key}` },
      });
      assert.equal(
        models.status,
        200,
        "profile proxy remains available with both windows closed",
      );
      stage(`c${cycle}:quota-popup`);
      click("Subscription quotas");
      await until(() =>
        BrowserWindow.getAllWindows().some((entry) => entry.isVisible()),
      );
      const popup = BrowserWindow.getAllWindows()[0];
      if (process.platform === "darwin") {
        popup.emit("blur");
        assert.equal(
          popup.isDestroyed(),
          false,
          "focus changes keep quotas open",
        );
      }
      if (cycle === 0) {
        app.emit("activate", {}, true);
        await delay(6000);
        assert.equal(popup.isDestroyed(), false, "idle quota popup stays open");
        assert.equal(BrowserWindow.getAllWindows().length, 1);
      }
      if (cycle === 1) click("Subscription quotas");
      else if (cycle === 2) {
        await run(popup, "window.nonstopvibin.showWindow()");
        await until(() => popup.isDestroyed());
        BrowserWindow.getAllWindows()[0].close();
      } else
        popup.webContents.sendInputEvent({
          type: "keyDown",
          keyCode: "Escape",
        });
      await until(() => BrowserWindow.getAllWindows().length === 0);
      // Await Chromium's process exit independently from BrowserWindow destruction.
      await until(() =>
        app.getAppMetrics().every((metric) => metric.type !== "Tab"),
      );
      samples.push({ phase: "closed", cycle, renderers: 0 });
    }
    if (process.platform === "darwin") {
      stage("outside-click");
      click("Subscription quotas");
      click("Subscription quotas");
      await until(() =>
        BrowserWindow.getAllWindows().some((window) => window.isVisible()),
      );
      assert.equal(
        monitors.length,
        4,
        "opening twice starts only one click monitor",
      );
      const popup = BrowserWindow.getAllWindows()[0];
      const monitor = monitors.at(-1);
      const bounds = popup.getBounds();
      monitor.stdout.emit(
        "data",
        Buffer.from(`${bounds.x + 10} ${bounds.y + 10}\n`),
      );
      assert.equal(
        popup.isDestroyed(),
        false,
        "inside clicks keep the popup open",
      );
      monitor.stdout.emit("data", Buffer.from("invalid\n"));
      assert.equal(
        popup.isDestroyed(),
        false,
        "invalid monitor output is ignored",
      );
      monitor.stdout.emit("data", Buffer.from("-100000 -100000\n"));
      await until(() => popup.isDestroyed());
      await until(() =>
        monitors.every(
          (child) => child.exitCode !== null || child.signalCode !== null,
        ),
      );
      samples.push({ phase: "outside-click", monitorsReleased: true });
    }
    stage("second-instance");
    app.emit("second-instance", {}, [], "");
    await until(() =>
      BrowserWindow.getAllWindows().some((entry) => entry.isVisible()),
    );
    stage("quit");
    writeFileSync(
      join(directory, "result.json"),
      JSON.stringify({ ok: true, samples }),
    );
    app.quit();
  }
  main().catch((error) => {
    writeFileSync(
      join(directory, "result.json"),
      JSON.stringify({ ok: false, error: error.stack }),
    );
    console.error(error);
    app.quit();
  });
}
