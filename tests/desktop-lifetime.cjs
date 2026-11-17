// Run by tests/desktop-smoke.cjs (`bun run test:desktop`); real Electron with isolated data checks that
// profile cores never outlive the app: SIGTERM, SIGINT and SIGHUP quit in order,
// and after SIGKILL the watchdog stops the core.
const assert = require("node:assert/strict");
const { execFileSync, spawn } = require("node:child_process");
const {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} = require("node:fs");
const { basename, join, resolve } = require("node:path");
const { tmpdir } = require("node:os");
const { setTimeout: delay } = require("node:timers/promises");
const electron = require("electron");

if (typeof electron === "string") {
  // Processes whose command line names this run's fresh temp directory; only this run creates those.
  const owned = (marker) =>
    execFileSync("ps", ["-ax", "-ww", "-o", "pid=,command="], {
      encoding: "utf8",
    })
      .split("\n")
      .map((line) => /^\s*(\d+) (.*)$/.exec(line))
      .filter(
        (match) =>
          match &&
          match[2].includes(marker) &&
          Number(match[1]) !== process.pid,
      )
      .map(([, pid, command]) => ({ pid: Number(pid), command }));
  async function until(check, label, timeout = 30_000) {
    const deadline = Date.now() + timeout;
    while (!(await check())) {
      if (Date.now() > deadline) throw new Error(`Timed out: ${label}`);
      await delay(100);
    }
  }
  async function scenario(signal) {
    const directory = mkdtempSync(join(tmpdir(), "nonstopvibin-lifetime-"));
    const marker = basename(directory);
    const launched = Date.now();
    const child = spawn(electron, [__filename, "--background"], {
      stdio: ["ignore", "inherit", "inherit"],
      env: {
        ...process.env,
        NONSTOPVIBIN_DATA_DIR: directory,
        NONSTOPVIBIN_PORT: "0",
      },
    });
    const exited = new Promise((done) =>
      child.on("exit", (code, signalCode) => done({ code, signalCode })),
    );
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
    } finally {
      if (child.exitCode === null && child.signalCode === null)
        child.kill("SIGKILL");
      for (const { pid } of owned(marker))
        try {
          process.kill(pid, "SIGKILL");
        } catch {
          // Already gone.
        }
      rmSync(directory, { recursive: true, force: true });
    }
  }
  (async () => {
    const results = [];
    for (const signal of ["SIGTERM", "SIGINT", "SIGHUP", "SIGKILL"])
      results.push(await scenario(signal));
    console.log("Desktop core lifetime passed:", JSON.stringify(results));
  })().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
} else {
  const { app, BrowserWindow, Menu } = electron;
  const directory = process.env.NONSTOPVIBIN_DATA_DIR;
  app.setAppPath(resolve(__dirname, ".."));
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
    await until(() => openItem());
    openItem().click();
    await until(() => BrowserWindow.getAllWindows().length > 0);
    const window = BrowserWindow.getAllWindows()[0];
    await until(() => !window.webContents.isLoading());
    const { token } = await window.webContents.executeJavaScript(
      "window.nonstopvibin.bootstrap()",
    );
    const origin = new URL(window.webContents.getURL()).origin;
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
    await request(`/profiles/${profile.id}/start`, "POST");
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
