// Shared by tests/desktop-smoke.cjs and tests/desktop-lifetime.cjs: a time limit for each
// Electron run, stage reporting from inside it, and evidence collection when it fails or stalls.
// Cleanup only touches the Electron child a test spawned, its descendants, and processes whose
// command line names that run's fresh temp directory.
const { execFileSync, spawn, spawnSync } = require("node:child_process");
const {
  appendFileSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync,
  writeSync,
} = require("node:fs");
const { basename, join, resolve } = require("node:path");
const { tmpdir } = require("node:os");
const { setTimeout: delay } = require("node:timers/promises");

/** Screenshots, app logs and stage records of failed runs; CI uploads this directory. */
const artifactRoot = resolve(__dirname, "../test-results/desktop-smoke");
// The fixed environment of the identity lookup in src/server/core-processes.ts.
const psEnvironment = {
  PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
  LC_ALL: "C",
  TZ: "UTC",
};

function launch(name, script, stdio) {
  const directory = mkdtempSync(join(tmpdir(), `nonstopvibin-${name}-`));
  const child = spawn(require("electron"), [script, "--background"], {
    stdio,
    env: {
      ...process.env,
      NONSTOPVIBIN_DATA_DIR: directory,
      NONSTOPVIBIN_PORT: "0",
      NONSTOPVIBIN_TEST_RUN: name,
    },
  });
  const exited = new Promise((done) => {
    child.on("exit", (code, signalCode) => done({ code, signalCode }));
    child.on("error", (error) => done({ error }));
  });
  return {
    name,
    child,
    directory,
    marker: basename(directory),
    launched: Date.now(),
    exited,
  };
}

const running = (child) => child.exitCode === null && child.signalCode === null;

/** Processes naming `marker`, plus `root` and its descendants when given. */
function processes(marker, root) {
  const all = execFileSync("ps", ["-ax", "-ww", "-o", "pid=,ppid=,command="], {
    encoding: "utf8",
    timeout: 5000,
  })
    .split("\n")
    .map((line) => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line))
    .filter(Boolean)
    .map(([, pid, ppid, command]) => ({
      pid: Number(pid),
      ppid: Number(ppid),
      command,
    }));
  const tree = new Set(root === undefined ? [] : [root]);
  for (let grew = true; grew;) {
    grew = false;
    for (const entry of all)
      if (!tree.has(entry.pid) && tree.has(entry.ppid)) {
        tree.add(entry.pid);
        grew = true;
      }
  }
  return all.filter(
    (entry) =>
      entry.pid !== process.pid &&
      (tree.has(entry.pid) || entry.command.includes(marker)),
  );
}

function killAll(run) {
  // Descendants only while the child runs: after it exits its PID could be reused.
  const root = running(run.child) ? run.child.pid : undefined;
  let found = [];
  try {
    found = processes(run.marker, root);
  } catch (error) {
    console.error("Could not list processes:", error.message);
  }
  if (root !== undefined) run.child.kill("SIGKILL");
  for (const { pid } of found)
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // Already gone.
    }
  return found.length;
}

/** Kills everything this run started and removes its temp directory. */
async function terminate(run) {
  for (let attempt = 0; attempt < 5 && killAll(run) > 0; attempt++)
    await delay(200);
  rmSync(run.directory, { recursive: true, force: true });
}

const records = (run) => {
  try {
    return readFileSync(join(run.directory, "stages.jsonl"), "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line));
  } catch {
    return [];
  }
};
const readJson = (path) => {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
};
const shorten = (text, length = 300) =>
  text.length > length ? `${text.slice(0, length)}…` : text;

/** Prints what is cheap and safe to learn about a stalled or failed run; every step is bounded. */
async function diagnose(run, reason) {
  const say = (...parts) => console.error(...parts);
  const seconds = ((Date.now() - run.launched) / 1000).toFixed(1);
  say(`--- ${run.name} diagnostics (${reason}, ${seconds}s after launch)`);
  const step = async (label, action) => {
    try {
      await action();
    } catch (error) {
      say(`${label}: could not collect (${error.message})`);
    }
  };
  const files = join(artifactRoot, run.name);
  mkdirSync(files, { recursive: true });
  await step("Stages", () => {
    const stages = records(run);
    if (!stages.length) return say("Stages: none reported");
    say(
      "Stages:",
      stages
        .map(
          (entry, index) =>
            `${entry.label} ${(((stages[index + 1]?.at ?? Date.now() - run.launched) - entry.at) / 1000).toFixed(1)}s`,
        )
        .join(", "),
    );
    say(`Last stage: ${stages.at(-1).label}`);
  });
  await step("Windows", async () => {
    const path = join(run.directory, "diagnostics.json");
    if (running(run.child)) {
      rmSync(path, { force: true });
      process.kill(run.child.pid, "SIGUSR2");
      for (let waited = 0; waited < 3000 && !existsSync(path); waited += 100)
        await delay(100);
    }
    const snapshot = readJson(path);
    say(
      "Electron main:",
      snapshot ? JSON.stringify(snapshot) : "no response within 3 s",
    );
  });
  await step("App state", async () => {
    const api = readJson(join(run.directory, "api.json"));
    if (!api) return say("App state: the test had not reached the API yet");
    const response = await fetch(`${api.origin}/api/state`, {
      headers: { Authorization: `Bearer ${api.token}` },
      signal: AbortSignal.timeout(3000),
    });
    const state = await response.json();
    say(
      "App state:",
      JSON.stringify({
        status: response.status,
        errors: state.errors,
        profiles: state.profiles?.map(
          (profile) =>
            `${profile.name}: ${profile.runtime}${profile.error ? ` (${profile.error})` : ""}`,
        ),
      }),
    );
  });
  let found = [];
  await step("Processes", () => {
    found = processes(
      run.marker,
      running(run.child) ? run.child.pid : undefined,
    );
    const cores = found.filter(
      (entry) =>
        entry.command.includes("/cli-proxy-api ") &&
        !entry.command.includes("nonstopvibin-core-watchdog"),
    );
    say(
      `Processes of this run: ${found.length}, profile cores: ${cores.length}, watchdogs: ${found.filter((entry) => entry.command.includes("nonstopvibin-core-watchdog")).length}`,
    );
    for (const entry of found)
      say(`  ${entry.pid} (parent ${entry.ppid}) ${shorten(entry.command)}`);
  });
  await step("Identity lookup", () => {
    const targets = [
      ...(running(run.child) ? [run.child.pid] : []),
      ...found
        .filter((entry) => entry.command.includes("/cli-proxy-api --config"))
        .map((entry) => entry.pid),
    ];
    for (const pid of targets) {
      const started = Date.now();
      const result = spawnSync(
        "ps",
        ["-ww", "-o", "lstart=", "-o", "command=", "-p", String(pid)],
        {
          env: psEnvironment,
          encoding: "utf8",
          timeout: 5000,
          killSignal: "SIGKILL",
        },
      );
      say(
        `ps identity of ${pid}: exit ${result.status ?? result.signal}${result.error ? ` (${result.error.message})` : ""} in ${Date.now() - started} ms, stdout ${JSON.stringify(shorten(result.stdout ?? ""))}, stderr ${JSON.stringify(result.stderr ?? "")}`,
      );
    }
  });
  await step("Screenshot", () => {
    if (process.platform !== "darwin") return;
    // A native dialog or permission prompt would show here; never wait on it.
    const result = spawnSync(
      "screencapture",
      ["-x", join(files, "screen.png")],
      {
        timeout: 5000,
        killSignal: "SIGKILL",
        stdio: "ignore",
      },
    );
    say(`Screenshot: exit ${result.status ?? result.signal ?? result.error}`);
  });
  await step("Files", () => {
    for (const name of [
      "logs",
      "crashDumps",
      "stages.jsonl",
      "diagnostics.json",
    ]) {
      const source = join(run.directory, name);
      if (!existsSync(source)) continue;
      if (lstatSync(source).isDirectory() && !readdirSync(source).length)
        continue;
      cpSync(source, join(files, name), { recursive: true });
    }
    say(`Files: ${files} (${readdirSync(files).join(", ") || "empty"})`);
  });
}

/** Ends the process after `ms` with diagnostics for the run `current()` returns. */
function limit(ms, label, current) {
  const timer = setTimeout(async () => {
    const run = current();
    if (run) run.expired = true; // Its exit is now this handler's to report.
    // Diagnostics are bounded, but nothing may outlive the limit by much.
    setTimeout(() => {
      if (run) {
        killAll(run);
        rmSync(run.directory, { recursive: true, force: true });
      }
      process.exit(1);
    }, 30_000).unref();
    console.error(`${label} exceeded its ${ms / 1000} s limit.`);
    if (run) {
      await diagnose(run, "time limit");
      await terminate(run);
    }
    process.exit(1);
  }, ms);
  return () => clearTimeout(timer);
}

/**
 * Inside the Electron child, before the app loads: prints each stage at once (unless quiet;
 * the parent prints recorded stages when a run fails), records it for the parent, answers SIGUSR2 with a window snapshot, and turns uncaught errors and the app's
 * error dialogs into an immediate failure instead of a modal dialog.
 */
function instrument(resultName, { quiet = false } = {}) {
  const { app, BrowserWindow, dialog } = require("electron");
  const directory = process.env.NONSTOPVIBIN_DATA_DIR;
  const name = process.env.NONSTOPVIBIN_TEST_RUN ?? "desktop";
  const started = Date.now();
  let current = "boot";
  const print = (text) => {
    try {
      writeSync(2, text); // Unbuffered: the line is out even if the main thread stalls next.
    } catch {
      process.stderr.write(text);
    }
  };
  const stage = (label) => {
    current = label;
    const at = Date.now() - started;
    if (!quiet) print(`${name} ${(at / 1000).toFixed(1)}s ${label}\n`);
    appendFileSync(
      join(directory, "stages.jsonl"),
      `${JSON.stringify({ label, at })}\n`,
    );
  };
  const snapshot = () => {
    const windows = BrowserWindow.getAllWindows().map((window) => ({
      id: window.id,
      title: window.getTitle(),
      visible: window.isVisible(),
      focused: window.isFocused(),
      modal: window.isModal(),
      children: window.getChildWindows().length,
      loading: window.webContents.isLoading(),
      crashed: window.webContents.isCrashed(),
      url: window.webContents.getURL(),
    }));
    const path = join(directory, "diagnostics.json");
    writeFileSync(
      `${path}.tmp`,
      JSON.stringify({
        stage: current,
        seconds: (Date.now() - started) / 1000,
        ready: app.isReady(),
        windows,
        focused: BrowserWindow.getFocusedWindow()?.id ?? null,
        metrics: app.getAppMetrics().map((metric) => metric.type),
      }),
    );
    renameSync(`${path}.tmp`, path);
  };
  process.on("SIGUSR2", snapshot);
  const fail = (kind) => (error) => {
    const detail = `${kind} during ${current}: ${error?.stack ?? error}`;
    print(`${name}: ${detail}\n`);
    try {
      writeFileSync(
        join(directory, resultName),
        JSON.stringify({ ok: false, error: detail }),
      );
      snapshot();
    } catch {
      // The failure itself is already printed.
    }
    app.exit(1);
  };
  // Any listener stops Electron's "A JavaScript error occurred" dialog; Node's own
  // default would turn an unhandled rejection into that uncaught exception.
  process.on("uncaughtException", fail("Uncaught exception"));
  process.on("unhandledRejection", fail("Unhandled rejection"));
  // The app's own error boxes are modal and print nothing on macOS.
  dialog.showErrorBox = (title, content) =>
    fail("Error dialog")(new Error(`${title}: ${content}`));
  stage("boot");
  return {
    stage,
    api: (origin, token) =>
      writeFileSync(
        join(directory, "api.json"),
        JSON.stringify({ origin, token }),
      ),
  };
}

module.exports = {
  artifactRoot,
  diagnose,
  instrument,
  launch,
  limit,
  processes,
  terminate,
};
