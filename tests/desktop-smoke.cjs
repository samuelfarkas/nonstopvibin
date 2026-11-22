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
const harness = require("./support/desktop-harness.cjs");

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
    require("./desktop-lifetime.cjs");
  })();
} else {
  const { stage, api } = harness.instrument("result.json");
  const { app, BrowserWindow } = electron;
  const directory = process.env.NONSTOPVIBIN_DATA_DIR;
  const samples = [];
  const monitors = [];
  const childProcess = require("node:child_process");
  const { PassThrough } = require("node:stream");
  const { createInterface } = require("node:readline");
  const spawn = childProcess.spawn;
  // The real monitor still starts, says "ready" and is stopped by the app, but the clicks it
  // reports (the person at the machine) never reach the app: only lines this test injects do.
  childProcess.spawn = (...args) => {
    const child = spawn(...args);
    if (!String(args[0]).endsWith("/tray-click-monitor")) return child;
    const lines = new PassThrough();
    createInterface({ input: child.stdout }).on("line", (line) => {
      if (line === "ready") lines.write("ready\n");
    });
    child.stdout.once("end", () => lines.end());
    child.stdout = lines;
    monitors.push(child);
    return child;
  };
  // Counts the renderer's release requests (useKeepDraft in client/controls.tsx) once main
  // has handled each; the app signals nothing else when it decides about a deferred close.
  let releases = 0;
  const handle = electron.ipcMain.handle.bind(electron.ipcMain);
  electron.ipcMain.handle = (channel, listener) =>
    handle(
      channel,
      channel === "nv:release-window"
        ? (...args) => {
            try {
              return listener(...args);
            } finally {
              releases++;
            }
          }
        : listener,
    );
  app.setAppPath(resolve(__dirname, ".."));
  stage("load-main");
  require("../dist/desktop/main.cjs");
  void app.whenReady().then(() => stage("app-ready"));
  const until = harness.deadline(50_000);
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
      (contents) =>
        // Electron hides what a renderer script threw; name the script that did.
        contents.executeJavaScript(code).catch((error) => {
          throw new Error(`${error.message}\nRenderer script: ${code}`);
        }),
    );
  const present = (window, selector) =>
    until(() =>
      run(
        window,
        `Boolean(document.querySelector(${JSON.stringify(selector)}))`,
      ),
    );
  async function press(window, selector) {
    await present(window, selector);
    await run(
      window,
      `document.querySelector(${JSON.stringify(selector)}).click()`,
    );
  }
  // Runs `action`, waits until main has handled the release it sends, and says whether main
  // began closing the window in response ("close" is emitted inside `window.close()`).
  async function release(window, action) {
    const before = releases;
    let closing = false;
    const began = () => {
      closing = true;
    };
    window.on("close", began);
    try {
      await action();
      await until(() => releases > before);
      return closing;
    } finally {
      window.off("close", began);
    }
  }
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
  async function closeGuarded(window, count, retained) {
    await until(async () => (await guards(window)) >= count);
    window.close();
    await until(() => {
      assert.equal(window.isDestroyed(), false, retained);
      return !window.isVisible();
    });
  }
  const click = (label) => {
    const item = harness.viewItem(label);
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
    await until(() => harness.viewItem("Open main window"));
    assert.equal(
      BrowserWindow.getAllWindows().length,
      0,
      "background startup creates no windows",
    );
    assert.deepEqual(
      app.getAppMetrics().filter((metric) => metric.type === "Tab"),
      [],
      "background startup launches no renderer",
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
        const request = harness.management(origin, token);
        const profile = await request("/profiles", "POST", {
          name: "Desktop fixture",
        });
        await present(window, ".proxy-card button");
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
        // No release is expected, so nothing marks its absence: one later than 100 ms goes unseen.
        await delay(100);
        assert.equal(
          window.isDestroyed(),
          false,
          "background failure remains available to read",
        );
        app.emit("activate");
        await until(() => window.isVisible());
        assert.equal(
          await release(window, () =>
            press(window, '[aria-label="Dismiss error"]'),
          ),
          false,
          "reopening cancels deferred close",
        );
        assert.equal(
          window.isVisible(),
          true,
          "reopening cancels deferred close",
        );
        stage("c0:start-profile");
        await request(`/profiles/${profile.id}/start`, "POST");
        const { key } = await request(`/profiles/${profile.id}/key`);
        proxy = { endpoint: `${origin}/p/${profile.slug}/v1`, key };
        // Exercise the real modal beforeunload guard, then discard through its close button.
        stage("c0:unsaved-form");
        await press(window, '[aria-label="New profile"]');
        await closeGuarded(window, 1, "unfinished form is retained");
        assert.equal(
          window.isDestroyed(),
          false,
          "unfinished form is retained",
        );
        app.emit("activate");
        await until(() => window.isVisible());
        assert.equal(
          await release(window, () =>
            press(window, '[aria-label="Close dialog"]'),
          ),
          false,
          "reopening cancels deferred close",
        );
      }
      stage(`c${cycle}:logo`);
      // The start screen draws a logo too, without the animation; wait for the sidebar's.
      await present(window, ".nv-logo-button .nv-ball");
      assert.equal(
        await run(
          window,
          `(async () => {
          const logo = document.querySelector('.nv-logo-button .nv-logo');
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
        await press(window, '[title="Settings"]');
        await present(window, "#usage-retention");
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
        await present(window, '[title="Settings fixture"]');
        const sidebarProfile = await run(
          window,
          "document.querySelector('.profiles [aria-current]').title",
        );
        await present(window, "#settings-profile");
        await run(
          window,
          `{
          const picker = document.querySelector('#settings-profile');
          picker.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
        }`,
        );
        await until(() =>
          run(
            window,
            `[...document.querySelectorAll('[role="option"]')].some(item => item.textContent.includes('Settings fixture'))`,
          ),
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
        await press(window, '[aria-label="Keep sessions on the same account"]');
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
        assert.equal(
          await release(window, () => retention(0)),
          false,
          "reopening cancels deferred close",
        );
        // One cleared guard must not discard another form in the same window.
        await retention(30);
        await press(window, '[aria-label="New profile"]');
        await closeGuarded(
          window,
          2,
          "settings and dialog drafts defer the close",
        );
        let vetoed = false;
        window.webContents.once("will-prevent-unload", () => {
          vetoed = true;
        });
        assert.equal(
          await release(window, () => retention(0)),
          true,
          "a cleared guard releases the hidden window",
        );
        await until(() => vetoed || window.isDestroyed());
        assert.equal(
          window.isDestroyed(),
          false,
          "remaining dialog keeps its draft",
        );
        await press(window, '[aria-label="Close dialog"]');
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
        // Idle for longer than the monitor's 5 s start limit (main.ts) with nothing to wait on.
        await delay(6000);
        assert.equal(popup.isDestroyed(), false, "idle quota popup stays open");
        assert.equal(BrowserWindow.getAllWindows().length, 1);
      }
      if (cycle === 1) click("Subscription quotas");
      else if (cycle === 2) {
        // A profile deleted after the tray rendered: the failure reaches the app's error list.
        const missing = require("node:crypto").randomUUID();
        assert.equal(
          await run(
            popup,
            `window.nonstopvibin.showWindow(${JSON.stringify(missing)}).then(() => "resolved", () => "rejected")`,
          ),
          "resolved",
          "a failed open does not reject in the tray",
        );
        const { errors } = await (
          await fetch(`${origin}/api/state`, {
            headers: { Authorization: `Bearer ${token}` },
          })
        ).json();
        assert.ok(
          errors.includes("Profile not found."),
          "a failed open is reported",
        );
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
      const started = monitors.length;
      click("Subscription quotas");
      click("Subscription quotas");
      await until(() =>
        BrowserWindow.getAllWindows().some((window) => window.isVisible()),
      );
      assert.equal(
        monitors.length,
        started + 1,
        "opening twice starts only one click monitor",
      );
      const popup = BrowserWindow.getAllWindows()[0];
      const monitor = monitors.at(-1);
      const bounds = popup.getBounds();
      // The app reads each line as it arrives and `close()` emits "close" at once.
      let closing = false;
      popup.once("close", () => {
        closing = true;
      });
      monitor.stdout.emit(
        "data",
        Buffer.from(`${bounds.x + 10} ${bounds.y + 10}\n`),
      );
      assert.equal(closing, false, "inside clicks keep the popup open");
      monitor.stdout.emit("data", Buffer.from("invalid\n"));
      assert.equal(closing, false, "invalid monitor output is ignored");
      monitor.stdout.emit("data", Buffer.from("-100000 -100000\n"));
      assert.equal(closing, true, "an outside click closes the popup");
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
