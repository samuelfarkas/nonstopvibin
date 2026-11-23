// Run with `bun tests/mac-desktop.mjs` on a GitHub Actions macOS runner after `bun run dist:mac`:
// the packaged app's desktop behaviour, driven as a user drives it. Mouse clicks are posted
// CoreGraphics events, keys and menus go through System Events (the runner grants osascript
// Accessibility), and every result is read back from the system: the menu bar item and its
// icon, the quota popup and how it closes, the tray menu, the main window coming to the front
// with key focus (docs/decisions/rust-port.md: 20 of 20), the Dock icon, the application menu,
// the login item, and quitting without leftovers. It uses the default data folder, as a second
// launch must find the running app there. Screenshots and results.json go to
// test-results/mac-desktop.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import sharp from "sharp";
import { product } from "../scripts/distribution.ts";

if (process.platform !== "darwin" || process.env.GITHUB_ACTIONS !== "true")
  throw new Error(
    "Run on a GitHub Actions macOS runner: osascript needs its Accessibility grant.",
  );

const root = resolve(import.meta.dirname, "..");
const results = join(root, "test-results/mac-desktop");
const name = product.name;
const packaged = join(root, `release/mac-${process.arch}/${name}.app`);
const apps = join(homedir(), "Applications");
const app = join(apps, `${name}.app`);
const macOS = join(app, "Contents/MacOS");
const daemonPath = join(macOS, name);
const helperPath = join(macOS, product.helper);
const MAIN = "NonstopVibin";
const QUOTA = "NonstopVibin quotas";
const TRIALS = 20;

rmSync(results, { recursive: true, force: true });
mkdirSync(results, { recursive: true });

// --- The system, as a user's tools see it -------------------------------------------------

function run(file, args, { allowFailure = false } = {}) {
  const result = spawnSync(file, args, { encoding: "utf8" });
  if (!allowFailure && result.status !== 0)
    throw new Error(
      `${file} ${args.join(" ")}: ${result.status}\n${result.stdout}${result.stderr}`,
    );
  return result.stdout.trim();
}
const appleScript = (script) => run("osascript", ["-e", script]);
const jxa = (script) => run("osascript", ["-l", "JavaScript", "-e", script]);
const json = (script) =>
  JSON.parse(jxa(`JSON.stringify((() => { ${script} })())`));

/** Processes whose executable is inside the installed app, as pid → command line. */
function appProcesses() {
  return run("ps", ["-ax", "-ww", "-o", "pid=,command="])
    .split("\n")
    .map((line) => line.trim().match(/^(\d+) (.*)$/))
    .filter((match) => match && match[2].startsWith(`${app}/`))
    .map(([, pid, command]) => ({ pid: Number(pid), command }));
}
const pidOf = (path) =>
  appProcesses().find(
    ({ command }) => command === path || command.startsWith(`${path} `),
  )?.pid;
/** WebKit's processes, each with the process launchd holds responsible for it. */
function webKitProcesses() {
  return run("ps", ["-ax", "-ww", "-o", "pid=,command="])
    .split("\n")
    .map((line) => line.trim().match(/^(\d+) (.*)$/))
    .filter((match) => match && match[2].includes("com.apple.WebKit."))
    .map(([, pid, command]) => {
      const info = run("sudo", ["-n", "launchctl", "procinfo", pid], {
        allowFailure: true,
      });
      return {
        pid: Number(pid),
        service: command.match(/com\.apple\.WebKit\.[A-Za-z]+/)?.[0],
        responsible: Number(info.match(/responsible pid = (\d+)/)?.[1]),
      };
    });
}

const SE = "const se = Application('System Events');";
const byPid = (pid) => `se.processes.whose({ unixId: ${pid} })[0]`;

/** The helper's windows by title, with their frames. */
function windows(pid = pidOf(helperPath)) {
  if (!pid) return [];
  return json(
    `${SE} return ${byPid(pid)}.windows().map((w) => ({ name: w.name(), position: w.position(), size: w.size() }));`,
  );
}
const windowNamed = (title) => windows().find((w) => w.name === title);

/** Every way to ask which app is in front, and the helper's focused window. */
function front() {
  const helper = pidOf(helperPath);
  const asn = run("lsappinfo", ["front"], { allowFailure: true });
  const lsPid = Number(
    run("lsappinfo", ["info", "-only", "pid", asn], {
      allowFailure: true,
    }).match(/=(\d+)/)?.[1],
  );
  const system = json(`
    ObjC.import('AppKit'); ObjC.import('CoreGraphics');
    const list = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly | $.kCGWindowListExcludeDesktopElements, 0)));
    const top = list.find((w) => w.kCGWindowLayer === 0);
    return { workspace: $.NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier, window: top && { pid: top.kCGWindowOwnerPID, name: top.kCGWindowName ?? null } };`);
  let focused = null;
  if (helper)
    focused = json(`${SE} const p = ${byPid(helper)};
      try { return { window: p.attributes.byName('AXFocusedWindow').value().name(), frontmost: p.frontmost() }; }
      catch (error) { return { window: null, frontmost: p.frontmost(), error: String(error) }; }`);
  return {
    helper,
    launchServices: lsPid,
    workspace: system.workspace,
    topWindow: system.window,
    focused,
  };
}
/** The main window is the frontmost window and the key window of the active app. */
const mainInFront = (state) =>
  state.helper !== undefined &&
  state.launchServices === state.helper &&
  state.topWindow?.pid === state.helper &&
  state.focused?.window === MAIN;

function screenshot(label) {
  run("screencapture", ["-x", join(results, `${label}.png`)], {
    allowFailure: true,
  });
}

// Posted mouse events: System Events' `click at` presses accessibility elements instead.
function click(x, y, button = "left") {
  const [down, up, which] =
    button === "right"
      ? [
          "kCGEventRightMouseDown",
          "kCGEventRightMouseUp",
          "kCGMouseButtonRight",
        ]
      : ["kCGEventLeftMouseDown", "kCGEventLeftMouseUp", "kCGMouseButtonLeft"];
  jxa(`ObjC.import('CoreGraphics'); const point = $.CGPointMake(${x}, ${y});
    for (const type of [$.kCGEventMouseMoved, $.${down}, $.${up}]) {
      $.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent($(), type, point, $.${which}));
      delay(0.05);
    }`);
}
/** Presses at one point, moves in steps, and releases at another. */
function drag(fromX, fromY, toX, toY) {
  jxa(`ObjC.import('CoreGraphics');
    const post = (type, x, y) => {
      $.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent($(), type, $.CGPointMake(x, y), $.kCGMouseButtonLeft));
      delay(0.02);
    };
    post($.kCGEventMouseMoved, ${fromX}, ${fromY});
    post($.kCGEventLeftMouseDown, ${fromX}, ${fromY});
    delay(0.15);
    for (let i = 1; i <= 10; i++)
      post($.kCGEventLeftMouseDragged, ${fromX} + (${toX - fromX} * i) / 10, ${fromY} + (${toY - fromY} * i) / 10);
    post($.kCGEventLeftMouseUp, ${toX}, ${toY});`);
}
const movePointer = (x, y) =>
  jxa(`ObjC.import('CoreGraphics');
    $.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent($(), $.kCGEventMouseMoved, $.CGPointMake(${x}, ${y}), 0));`);
const keyCode = (code) =>
  appleScript(`tell application "System Events" to key code ${code}`);

/** Polls `probe` until it returns something truthy or `ms` pass; its last value either way. */
async function poll(probe, ms) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await probe();
    if (value || Date.now() > deadline) return value;
    await delay(100);
  }
}
async function waitFor(what, probe, ms = 10_000) {
  const value = await poll(probe, ms);
  if (!value) throw new Error(`timed out waiting for ${what}`);
  return value;
}
/** The front state once the main window is in front, or after `ms`. */
async function frontWithin(ms) {
  let state;
  await poll(() => mainInFront((state = front())), ms);
  return state;
}

// --- The menu bar item ---------------------------------------------------------------------

/** The daemon's status item: the menu extra in its menu bars. */
function trayItem() {
  const daemon = pidOf(daemonPath);
  return json(`${SE} const p = ${byPid(daemon)};
    for (const bar of p.menuBars()) for (const item of bar.menuBarItems())
      if (item.subrole() === 'AXMenuExtra')
        return { position: item.position(), size: item.size(), description: item.description() };
    return null;`);
}
const centre = ({ position: [x, y], size: [w, h] }) => [x + w / 2, y + h / 2];
function clickTray(button = "left") {
  const [x, y] = centre(trayItem());
  click(x, y, button);
}
/** Opens the tray menu with a right click and returns its items' titles. */
async function trayMenu() {
  clickTray("right");
  const daemon = pidOf(daemonPath);
  return waitFor("the tray menu", () => {
    try {
      return json(`${SE} const p = ${byPid(daemon)};
        for (const bar of p.menuBars()) for (const item of bar.menuBarItems())
          if (item.subrole() === 'AXMenuExtra' && item.menus().length)
            return item.menus[0].menuItems.name();
        return null;`);
    } catch {
      return null;
    }
  });
}
function chooseTrayItem(title) {
  const daemon = pidOf(daemonPath);
  jxa(`${SE} const p = ${byPid(daemon)};
    for (const bar of p.menuBars()) for (const item of bar.menuBarItems())
      if (item.subrole() === 'AXMenuExtra') item.menus[0].menuItems.byName(${JSON.stringify(title)}).click();`);
}
async function fromTrayMenu(title) {
  await trayMenu();
  chooseTrayItem(title);
}

/** The icon's pixels, with the pointer away from the menu bar. */
async function trayPixels(label) {
  const {
    position: [x, y],
    size: [w, h],
  } = trayItem();
  movePointer(512, 500);
  await delay(300);
  const file = join(results, `tray-${label}.png`);
  run("screencapture", ["-x", `-R${x},${y},${w},${h}`, file]);
  const { data, info } = await sharp(file)
    .removeAlpha()
    .raw()
    .toBuffer({ resolveWithObject: true });
  return { data, info };
}
/** Mean channel spread (max - min) per pixel: 0 for greys, high for the coloured ball. */
function colour({ data }) {
  let total = 0;
  for (let i = 0; i < data.length; i += 3)
    total +=
      Math.max(data[i], data[i + 1], data[i + 2]) -
      Math.min(data[i], data[i + 1], data[i + 2]);
  return total / (data.length / 3);
}

// --- The rest of the desktop ---------------------------------------------------------------

const dockItems = () =>
  json(`${SE} return se.processes.byName('Dock').lists[0].uiElements.name();`);
function finderInFront() {
  appleScript(`tell application "Finder"
    activate
    if (count of Finder windows) is 0 then make new Finder window
  end tell`);
}
function closeMain() {
  const helper = pidOf(helperPath);
  jxa(`${SE} const w = ${byPid(helper)}.windows.byName('${MAIN}');
    w.buttons().find((b) => b.subrole() === 'AXCloseButton').click();`);
}
async function mainClosed() {
  await waitFor("the main window to close", () => !windowNamed(MAIN));
}

/** A web element in the main window by role and accessible name, found breadth first. */
function webElement(role, label) {
  return `${SE}
    const queue = [${byPid(pidOf(helperPath))}.windows.byName('${MAIN}')];
    let found = null;
    while (queue.length && !found) {
      const element = queue.shift();
      const children = element.uiElements;
      let roles, names, descriptions;
      try { roles = children.role(); names = children.name(); descriptions = children.description(); }
      catch { continue; }
      roles.forEach((role, i) => {
        if (!found && role === '${role}' && (names[i] === ${JSON.stringify(label)} || descriptions[i] === ${JSON.stringify(label)}))
          found = children[i];
        else queue.push(children[i]);
      });
    }
    if (!found) throw new Error('no ${role} ${label}');`;
}
function pressWeb(role, label) {
  jxa(`${webElement(role, label)} found.actions.byName('AXPress').perform();`);
}
function webValue(role, label) {
  return json(`${webElement(role, label)} return found.value();`);
}

function loginItems() {
  const dump = run("sudo", ["-n", "sfltool", "dumpbtm"], {
    allowFailure: true,
  });
  const url = `file://${app}/`;
  return dump
    .split(/\n\s*#\d+:\n/)
    .filter((block) => block.includes(url))
    .map((block) => block.match(/Disposition:\s*(\[[^\]]*\])/)?.[1] ?? block);
}

// An OpenAI-compatible upstream for the profile whose running state colours the icon.
const upstream = createServer((request, response) => {
  const body = request.url.includes("/models")
    ? { object: "list", data: [{ id: "stub-model", object: "model" }] }
    : { id: "stub", object: "chat.completion", choices: [] };
  response.writeHead(200, { "Content-Type": "application/json" });
  response.end(JSON.stringify(body));
});
async function api(method, path, body) {
  const session = JSON.parse(run(daemonPath, ["url", "--json"]));
  const url = new URL(session.url);
  const token = url.hash.slice("#session=".length);
  const response = await fetch(`${url.origin}/api${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: body && JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

// --- Checks ----------------------------------------------------------------------------------

const outcomes = [];
async function check(label, body) {
  const started = Date.now();
  try {
    const detail = await body();
    outcomes.push({ label, pass: true, ms: Date.now() - started, detail });
    console.log(`pass  ${label}`);
  } catch (error) {
    outcomes.push({
      label,
      pass: false,
      ms: Date.now() - started,
      error: String(error?.stack ?? error),
    });
    console.log(`FAIL  ${label}\n      ${error}`);
    screenshot(`failed-${outcomes.length}`);
  }
}
function equal(actual, expected, what) {
  if (JSON.stringify(actual) !== JSON.stringify(expected))
    throw new Error(
      `${what}: ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`,
    );
}
function ok(value, what, detail) {
  if (!value)
    throw new Error(`${what}${detail ? `: ${JSON.stringify(detail)}` : ""}`);
}

/** Brings the main window up `how` while Finder is in front, `trials` times. */
async function frontTrials(label, how) {
  const trials = [];
  for (let i = 0; i < TRIALS; i++) {
    // Half the trials start a new helper, half bring an existing window back.
    const cold = i % 2 === 0;
    if (cold && windowNamed(MAIN)) {
      closeMain();
      await mainClosed();
    }
    finderInFront();
    await delay(400);
    const started = Date.now();
    await how();
    const state = await frontWithin(5000);
    trials.push({
      cold,
      pass: mainInFront(state),
      ms: Date.now() - started,
      state,
    });
    if (!mainInFront(state)) screenshot(`${label}-trial-${i}`);
  }
  const passed = trials.filter((trial) => trial.pass).length;
  ok(
    passed === TRIALS,
    `${passed}/${TRIALS} in front with key focus`,
    trials.filter((t) => !t.pass),
  );
  return { passed, trials };
}

await new Promise((done) => upstream.listen(0, "127.0.0.1", done));
rmSync(app, { recursive: true, force: true });
mkdirSync(apps, { recursive: true });
run("ditto", [packaged, app]);
let exit = 1;
try {
  run("open", [app]);

  await check("the first launch shows the main window in front", async () => {
    await waitFor("the main window", () => windowNamed(MAIN), 30_000);
    const state = await frontWithin(10_000);
    screenshot("first-launch");
    ok(mainInFront(state), "in front with key focus", state);
    return state;
  });

  await check("the menu bar item exists", async () => {
    const item = await waitFor("the menu bar item", () => trayItem());
    ok(item.position[1] < 40, "it is in the menu bar", item);
    return item;
  });

  await check(
    "the Dock icon shows only while the main window exists",
    async () => {
      const withMain = dockItems();
      ok(withMain.includes(name), "shown with the main window", withMain);
      closeMain();
      await mainClosed();
      await waitFor("the helper to exit", () => !pidOf(helperPath));
      await delay(500);
      const without = dockItems();
      ok(!without.includes(name), "hidden without a window", without);
      return { withMain, without };
    },
  );

  await check("the menu bar icon follows the running state", async () => {
    const stopped = colour(await trayPixels("stopped"));
    const created = await api("POST", "/profiles", { name: "Personal" });
    equal(created.status, 201, "profile created");
    const id = created.body.id;
    const account = await api("POST", `/profiles/${id}/api-account`, {
      name: "Desktop check",
      provider: "custom",
      baseUrl: `http://127.0.0.1:${upstream.address().port}/v1`,
      apiKey: "synthetic-key",
      prefix: "",
      models: [{ id: "stub-model", protocol: "openai" }],
    });
    equal(account.status, 201, "account added");
    await api("POST", `/profiles/${id}/start`);
    await waitFor(
      "the profile to run",
      async () =>
        (await api("GET", "/state")).body.profiles[0]?.runtime === "running",
      60_000,
    );
    await delay(1000);
    const running = colour(await trayPixels("running"));
    const item = trayItem();
    ok(running > stopped + 10, "the running icon is coloured", {
      stopped,
      running,
    });
    return { stopped, running, description: item.description };
  });

  await check(
    "a click on the item opens the quota popup below it",
    async () => {
      const item = trayItem();
      clickTray();
      const popup = await waitFor("the quota popup", () => windowNamed(QUOTA));
      await delay(300);
      screenshot("quota-popup");
      const [itemX] = centre(item);
      const itemBottom = item.position[1] + item.size[1];
      const gap = popup.position[1] - itemBottom;
      ok(gap >= 0 && gap <= 12, "the popup's top is just below the item", {
        item,
        popup,
      });
      ok(
        popup.position[0] <= itemX &&
          itemX <= popup.position[0] + popup.size[0],
        "the popup spans the item horizontally",
        { item, popup },
      );
      const dock = dockItems();
      ok(!dock.includes(name), "no Dock icon for the popup alone", dock);
      return { item, popup };
    },
  );

  await check("Escape closes the popup", async () => {
    const focused = await poll(() => front().focused?.window === QUOTA, 3000);
    ok(focused, "the popup has the key focus", front());
    keyCode(53);
    await waitFor("the popup to close", () => !windowNamed(QUOTA), 3000);
  });

  await check("a click elsewhere closes the popup", async () => {
    clickTray();
    await waitFor("the quota popup", () => windowNamed(QUOTA));
    await delay(500);
    click(300, 450);
    await waitFor("the popup to close", () => !windowNamed(QUOTA), 3000);
  });

  await check(
    "a tray click that takes the focus from the popup does not reopen it",
    async () => {
      clickTray();
      await waitFor("the quota popup", () => windowNamed(QUOTA));
      await delay(500);
      clickTray();
      await waitFor("the popup to close", () => !windowNamed(QUOTA), 3000);
      await delay(1500);
      ok(!windowNamed(QUOTA), "the popup stays closed");
      clickTray();
      await waitFor("a later click to open it", () => windowNamed(QUOTA), 5000);
      keyCode(53);
      await waitFor("the popup to close", () => !windowNamed(QUOTA), 3000);
    },
  );

  await check("the tray menu lists main.ts's items", async () => {
    const items = await trayMenu();
    screenshot("tray-menu");
    keyCode(53);
    equal(
      items,
      [
        "Open NonstopVibin",
        "Subscription quotas",
        "Check for updates…",
        null,
        "Quit NonstopVibin",
      ],
      "items",
    );
    return items;
  });

  await check("Subscription quotas opens the popup", async () => {
    await fromTrayMenu("Subscription quotas");
    await waitFor("the quota popup", () => windowNamed(QUOTA));
    keyCode(53);
    await waitFor("the popup to close", () => !windowNamed(QUOTA), 3000);
  });

  await check(
    "the main window moves when dragged by its top edge",
    async () => {
      if (!windowNamed(MAIN)) {
        await fromTrayMenu("Open NonstopVibin");
        await waitFor("the main window", () => windowNamed(MAIN), 10_000);
      }
      await delay(500);
      const before = windowNamed(MAIN);
      const [x, y] = [
        before.position[0] + before.size[0] - 200,
        before.position[1] + 12,
      ];
      drag(x, y, x - 60, y + 40);
      await delay(500);
      const after = windowNamed(MAIN);
      ok(
        after.position[0] === before.position[0] - 60 &&
          after.position[1] === before.position[1] + 40,
        "the window followed the pointer",
        { before: before.position, after: after.position },
      );
      return { before: before.position, after: after.position };
    },
  );

  await check(
    `Open NonstopVibin brings the main window to the front (${TRIALS} trials)`,
    () => frontTrials("tray", () => fromTrayMenu("Open NonstopVibin")),
  );

  await check(
    `a second launch (open -n) brings the main window to the front (${TRIALS} trials)`,
    () => frontTrials("second-launch", () => run("open", ["-n", app])),
  );

  await check(
    `opening the running app again (open) brings the main window to the front (${TRIALS} trials)`,
    () => frontTrials("reopen", () => run("open", [app])),
  );

  await check(
    "the login switch registers and unregisters the app",
    async () => {
      if (!windowNamed(MAIN)) {
        await fromTrayMenu("Open NonstopVibin");
        await waitFor("the main window", () => windowNamed(MAIN), 10_000);
      }
      // The window shows "Opening your workspace…" until the app has loaded.
      await waitFor(
        "the Settings button",
        () => {
          try {
            pressWeb("AXButton", "Settings");
            return true;
          } catch {
            return false;
          }
        },
        30_000,
      );
      const before = await waitFor("the Start at login switch", () => {
        try {
          return { value: webValue("AXCheckBox", "Start at login") };
        } catch {
          return null;
        }
      });
      equal(before.value, 0, "off at first");
      equal(
        loginItems().filter((d) => d.includes("enabled")).length,
        0,
        "not registered at first",
      );
      pressWeb("AXCheckBox", "Start at login");
      await waitFor(
        "the switch to turn on",
        () => webValue("AXCheckBox", "Start at login") === 1,
        5000,
      );
      const registered = loginItems();
      screenshot("login-on");
      ok(
        registered.some((d) => d.includes("enabled")),
        "SMAppService lists the app enabled",
        registered,
      );
      // getLoginItem: a new window reads the registration back.
      closeMain();
      await mainClosed();
      await fromTrayMenu("Open NonstopVibin");
      await waitFor("the main window", () => windowNamed(MAIN), 10_000);
      const reread = await waitFor("the switch to read on", () => {
        try {
          return webValue("AXCheckBox", "Start at login") === 1;
        } catch {
          return false;
        }
      });
      pressWeb("AXCheckBox", "Start at login");
      await waitFor(
        "the switch to turn off",
        () => webValue("AXCheckBox", "Start at login") === 0,
        5000,
      );
      const unregistered = loginItems();
      ok(
        !unregistered.some((d) => d.includes("enabled")),
        "SMAppService no longer lists it enabled",
        unregistered,
      );
      return { registered, reread, unregistered };
    },
  );

  await check(
    "Quit NonstopVibin leaves no daemon, helper, core or WebKit process",
    async () => {
      const running = appProcesses();
      const helper = pidOf(helperPath);
      ok(helper, "the helper runs before quitting");
      // launchd holds the daemon, which started the helper, responsible for them.
      const owners = [pidOf(daemonPath), helper];
      const before = webKitProcesses();
      const helpers = before.filter((p) => owners.includes(p.responsible));
      ok(helpers.length > 0, "the helper's WebKit processes are known", {
        owners,
        before,
      });
      await fromTrayMenu("Quit NonstopVibin");
      await poll(() => appProcesses().length === 0, 20_000);
      const left = appProcesses();
      const alive = () => {
        const now = webKitProcesses().map((p) => p.pid);
        return helpers.filter((p) => now.includes(p.pid));
      };
      await poll(() => alive().length === 0, 10_000);
      ok(left.length === 0, "app processes left", left);
      ok(alive().length === 0, "the helper's WebKit processes outlived it", {
        helpers,
        after: webKitProcesses(),
      });
      return {
        running: running.map(({ command }) => command.slice(app.length)),
        helpers,
        others: before.filter((p) => !owners.includes(p.responsible)),
      };
    },
  );

  exit = outcomes.every((outcome) => outcome.pass) ? 0 : 1;
} finally {
  writeFileSync(
    join(results, "results.json"),
    `${JSON.stringify(outcomes, null, 2)}\n`,
  );
  // Leftovers already failed the quit check; stop them so the next steps start clean.
  const left = appProcesses().map(({ pid }) => String(pid));
  if (left.length) run("kill", ["-9", ...left], { allowFailure: true });
  upstream.close();
  rmSync(app, { recursive: true, force: true });
  if (existsSync(results))
    console.log(
      `${outcomes.filter((o) => o.pass).length}/${outcomes.length} checks passed`,
    );
}
process.exit(exit);
