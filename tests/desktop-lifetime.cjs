// Run by tests/desktop-smoke.cjs (`bun run test:desktop`); real Electron with isolated data checks that
// profile cores never outlive the app: SIGTERM, SIGINT and SIGHUP quit in order,
// and after SIGKILL the watchdog stops the core.
const assert = require("node:assert/strict");
const {
  existsSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} = require("node:fs");
const { join, resolve } = require("node:path");
const { setTimeout: delay } = require("node:timers/promises");
const electron = require("electron");
const harness = require("./desktop-harness.cjs");

if (typeof electron === "string") {
  // Processes whose command line names this run's fresh temp directory; only this run creates those.
  const owned = (marker) =>
    harness.processes(marker).map(({ pid, command }) => ({ pid, command }));
  async function until(check, label, timeout = 30_000) {
    const deadline = Date.now() + timeout;
    while (!(await check())) {
      if (Date.now() > deadline) throw new Error(`Timed out: ${label}`);
      await delay(100);
    }
  }
  // Four launches take about 15 s locally; each one's own waits end within 80 s.
  const limit = 150_000;
  let active;
  async function scenario(signal) {
    const run = harness.launch(`lifetime-${signal}`, __filename, [
      "ignore",
      "inherit",
      "inherit",
    ]);
    active = run;
    const { child, directory, marker, launched, exited } = run;
    try {
      const ready = join(directory, "ready.json");
      await until(() => existsSync(ready), "profile core running", 50_000);
      console.log(
        `${signal}: profile core running after ${Date.now() - launched} ms`,
      );
      const result = JSON.parse(readFileSync(ready, "utf8"));
      assert.equal(result.ok, true, result.error);
      const ledger = join(directory, "core-processes");
      const running = owned(marker);
      const isWatchdog = (p) =>
        p.command.includes("nonstopvibin-core-watchdog");
      const watchdogs = running.filter(isWatchdog);
      const cores = running.filter(
        (p) => !isWatchdog(p) && p.command.includes("/cli-proxy-api "),
      );
      assert.equal(cores.length, 1, "one profile core runs");
      assert.ok(watchdogs.length >= 1, "the core has a watchdog");
      assert.deepEqual(readdirSync(ledger), [`${cores[0].pid}.json`]);
      const signalled = Date.now();
      child.kill(signal);
      if (signal !== "SIGKILL") {
        const stuck = setTimeout(() => child.kill("SIGKILL"), 20_000);
        const { code, signalCode } = await exited;
        clearTimeout(stuck);
        assert.equal(code, 0, `orderly quit after ${signal} (${signalCode})`);
        assert.deepEqual(
          owned(marker).map((p) => p.command),
          [],
          `no core, watchdog or helper remains after ${signal}`,
        );
        assert.deepEqual(readdirSync(ledger), [], "the ledger is empty");
      } else {
        const { signalCode } = await exited;
        assert.equal(signalCode, "SIGKILL");
        await until(
          () =>
            !owned(marker).some(
              (p) =>
                p.command.includes("cli-proxy-api") ||
                p.command.includes("nonstopvibin-core-watchdog"),
            ),
          "core stopped after its owner was killed",
          8_000,
        );
      }
      return { signal, milliseconds: Date.now() - signalled };
    } catch (error) {
      if (!run.expired) {
        console.error(error);
        await harness.diagnose(run, "failure");
      }
      throw error;
    } finally {
      await harness.terminate(run);
      active = undefined;
    }
  }
  const cancel = harness.limit(limit, "Desktop core lifetime", () => active);
  (async () => {
    const results = [];
    for (const signal of ["SIGTERM", "SIGINT", "SIGHUP", "SIGKILL"])
      results.push(await scenario(signal));
    console.log("Desktop core lifetime passed:", JSON.stringify(results));
  })()
    .catch(() => {
      process.exitCode = 1; // The failing scenario printed its error and diagnostics.
    })
    .finally(cancel);
} else {
  const { stage, api } = harness.instrument("ready.json", { quiet: true });
  const { app, BrowserWindow, Menu } = electron;
  const directory = process.env.NONSTOPVIBIN_DATA_DIR;
  app.setAppPath(resolve(__dirname, ".."));
  stage("load-main");
  require("../dist/desktop/main.cjs");
  const deadline = Date.now() + 40_000;
  async function until(check) {
    while (!(await check())) {
      if (Date.now() > deadline) throw new Error("Desktop check timed out");
      await delay(50);
    }
  }
  const openItem = () =>
    Menu.getApplicationMenu()
      ?.items.find((item) => item.label === "View")
      ?.submenu.items.find((item) => item.label === "Open main window");
  async function main() {
    stage("wait-menu");
    await until(() => openItem());
    stage("open-main");
    openItem().click();
    await until(() => BrowserWindow.getAllWindows().length > 0);
    const window = BrowserWindow.getAllWindows()[0];
    await until(() => !window.webContents.isLoading());
    const { token } = await window.webContents.executeJavaScript(
      "window.nonstopvibin.bootstrap()",
    );
    const origin = new URL(window.webContents.getURL()).origin;
    api(origin, token);
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
    const profile = await request("/profiles", "POST", { name: "Lifetime" });
    stage("start-profile");
    await request(`/profiles/${profile.id}/start`, "POST");
    stage("ready");
    window.close();
    writeFileSync(join(directory, "ready.json"), JSON.stringify({ ok: true }));
  }
  main().catch((error) => {
    writeFileSync(
      join(directory, "ready.json"),
      JSON.stringify({ ok: false, error: error.stack }),
    );
  });
}
