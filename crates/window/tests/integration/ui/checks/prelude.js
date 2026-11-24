// Runs ahead of every page script in a UI check (tests/ui/site.rs puts it there). A check
// file follows it and calls `run(async () => { ... })`; the first failed assertion ends the
// check and reaches the test with its label.
"use strict";
const send = window.fetch.bind(window);
const report = (step, data = {}) =>
  send("/__report", {
    method: "POST",
    body: JSON.stringify({ step, ...data }),
  });
// A native action the test performs (a key, a click, a resize, closing or showing the
// window) before it answers.
const act = async (action) =>
  (
    await send("/__act", { method: "POST", body: JSON.stringify(action) })
  ).json();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

class Failure extends Error {
  constructor(label, actual, expected) {
    super(label);
    Object.assign(this, { label, actual, expected });
  }
}
// Equal as JSON, whatever the order of object keys.
const canonical = (value) =>
  JSON.stringify(value, (_, item) =>
    item && typeof item === "object" && !Array.isArray(item)
      ? Object.fromEntries(
          Object.entries(item).sort(([a], [b]) => (a < b ? -1 : 1)),
        )
      : item,
  );
const same = (a, b) => canonical(a) === canonical(b);
const equal = (actual, expected, label) => {
  if (!same(actual, expected)) throw new Failure(label, actual, expected);
};
const ok = (value, label) => equal(Boolean(value), true, label);
const match = (text, pattern, label) => {
  if (!pattern.test(text)) throw new Failure(label, text, String(pattern));
};
async function until(ready, label, ms = 15000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await ready();
    if (value) return value;
    if (Date.now() > deadline)
      throw new Failure(`${label} (timed out)`, value, true);
    await sleep(20);
  }
}

const $ = (selector) => document.querySelector(selector);
const outline = () => getComputedStyle(document.activeElement).outlineStyle;
// Resolves once the page has seen `type`, so what the input did is in the DOM.
async function sees(type, label, input) {
  let seen = false;
  const mark = () => (seen = true);
  document.addEventListener(type, mark, { capture: true, once: true });
  await act(input);
  await until(() => seen, `${label} reaches the page`, 5000);
}
const key = (name) =>
  sees("keyup", `the ${JSON.stringify(name)} key`, { key: name });
// A real mouse click at the middle of the element. `top` is how far below the window's top
// the page starts: the height of the Linux main window's menu bar.
async function click(selector) {
  const { x, y, width, height } = (
    await present(selector)
  ).getBoundingClientRect();
  await sees("mouseup", `a click on ${selector}`, {
    click: [x + width / 2, y + height / 2],
    top: outerHeight - innerHeight,
  });
}
function present(selector) {
  return until(() => $(selector), `${selector} appears`);
}
async function press(selector) {
  (await present(selector)).click();
}

// WebKit has no media emulation: rewrite the stylesheets' `(feature: value)` conditions so
// those for `value` hold and the others do not. Each call starts from a rule's own text, so
// a later call can choose another value.
const mediaTexts = new WeakMap();
function emulateMedia(feature, value) {
  const condition = new RegExp(
    `\\(\\s*${feature}\\s*:\\s*([a-z-]+)\\s*\\)`,
    "g",
  );
  const visit = (rules) => {
    for (const rule of rules) {
      if (rule instanceof CSSMediaRule) {
        if (!mediaTexts.has(rule)) mediaTexts.set(rule, rule.media.mediaText);
        const text = mediaTexts.get(rule);
        const emulated = text.replace(condition, (_, wanted) =>
          wanted === value ? "(min-width: 0px)" : "(min-width: 99999px)",
        );
        if (emulated !== text) rule.media.mediaText = emulated;
      }
      if (rule.cssRules) visit(rule.cssRules);
    }
  };
  for (const sheet of document.styleSheets) visit(sheet.cssRules);
}

// How often the window asked the page whether it may close.
let unloadAsks = 0;
window.addEventListener("beforeunload", () => unloadAsks++);
// Live beforeunload listeners: the draft guards useKeepDraft installs.
const guarding = new Set();
{
  const add = window.addEventListener;
  const remove = window.removeEventListener;
  window.addEventListener = function (type, listener, ...rest) {
    if (type === "beforeunload") guarding.add(listener);
    return add.call(this, type, listener, ...rest);
  };
  window.removeEventListener = function (type, listener, ...rest) {
    if (type === "beforeunload") guarding.delete(listener);
    return remove.call(this, type, listener, ...rest);
  };
}
const guards = () => guarding.size;

const uncaught = [];
window.addEventListener("error", (event) =>
  uncaught.push(String(event.message)),
);
window.addEventListener("unhandledrejection", (event) =>
  uncaught.push(`Uncaught (in promise) ${event.reason}`),
);

// Checks run in the top page; a page it frames only gets the helpers above.
function run(check) {
  if (window.top !== window) return;
  const start = async () => {
    try {
      // `after` runs once the result is in: a last step that ends the page.
      const { after, ...result } = (await check()) ?? {};
      await report("done", result);
      after?.();
    } catch (error) {
      const { label = String(error), actual, expected } = error;
      await report("failed", { label, actual, expected, stack: error.stack });
    }
  };
  if (document.readyState === "loading")
    document.addEventListener("DOMContentLoaded", start);
  else void start();
}

// The desktop app's own helpers: the session, the management API, and the window.
let token = "";
async function api(path, method = "GET", body) {
  const response = await send(`/api${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: body && JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}
// The token a new window bootstraps with is the daemon's.
async function bootstrap() {
  token = (await window.nonstopvibin.bootstrap()).token;
  equal(
    (await api("/state")).status,
    200,
    "bootstrap accepts the recreated renderer",
  );
}
// Says which step comes next, so a window that closes under it names it.
const progress = (label) => report("progress", { label });
// Closes the window with drafts open: it must hide, not close.
async function closeGuarded(count, label) {
  await until(() => guards() >= count, `${count} draft guard(s) installed`);
  await progress(label);
  equal((await act({ close: true })).visible, false, label);
}
// Runs `action` and waits for the release it sends: whether the window asked the page to
// close again, and whether it is still on screen.
async function released(action) {
  const asks = unloadAsks;
  await action();
  const { visible } = await act({ settle: 300 });
  return { asked: unloadAsks > asks, visible };
}
// Picks `label` in a select the way the keyboard does.
async function choose(selector, label) {
  $(selector).dispatchEvent(
    new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
  );
  const option = await until(
    () =>
      [...document.querySelectorAll('[role="option"]')].find((item) =>
        label(item.textContent),
      ),
    `${selector} lists the option`,
  );
  option.focus();
  option.dispatchEvent(
    new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
  );
  await until(
    () => $(selector).getAttribute("aria-expanded") === "false",
    `${selector} closes its list`,
  );
}
const retention = (days) =>
  choose(
    "#usage-retention",
    (text) =>
      text ===
      { 0: "Keep all history", 30: "30 days", 90: "90 days", 365: "1 year" }[
        days
      ],
  );
// The sidebar's logo plays a finite animation and then rests.
async function logoRests() {
  // The start screen draws a logo too, without the animation; wait for the sidebar's.
  await present(".nv-logo-button .nv-ball");
  const logo = $(".nv-logo-button .nv-logo");
  const animations = logo.getAnimations({ subtree: true });
  const infinite = animations.some(
    (animation) => animation.effect.getTiming().iterations === Infinity,
  );
  await Promise.all(animations.map((animation) => animation.finished));
  equal(
    infinite ? -1 : logo.getAnimations({ subtree: true }).length,
    0,
    "logo finishes its finite animation and stays idle",
  );
}
