// The harness page. It runs under the production CSP (no inline script), exercises
// window.nonstopvibin and reports each step to POST /result. Its mode follows the URL the
// helper opened: / (main), /?view=tray, /?profile=<uuid>, the nil profile (origin probe), a
// sandboxed frame inside the probe, or the stray page a cross-origin redirect leads to.
const report = (step, data = {}) =>
  fetch("/result", { method: "POST", body: JSON.stringify({ step, ...data }) });
const settle = (promise) =>
  promise.then(
    (value) => ({ value: value ?? null }),
    (error) => ({ error: String(error.message) }),
  );
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const next = () => fetch("/next").then((response) => response.text());
const bridge = window.nonstopvibin;
// A message the page itself posts to the helper's IPC channel, without the script's nonce.
const forge = (method) => {
  try {
    window.webkit.messageHandlers.ipc.postMessage(
      JSON.stringify({ nonce: "forged", id: 1, method }),
    );
    return "posted";
  } catch (error) {
    return String(error);
  }
};
const profile = new URLSearchParams(location.search).get("profile");

async function main() {
  await report("loaded", {
    bridge: typeof bridge,
    methods: Object.keys(bridge).sort(),
    frozen: Object.isFrozen(bridge),
  });
  const calls = {
    bootstrap: await settle(bridge.bootstrap()),
    copy: await settle(bridge.copy("copied by the bridge ✓")),
    releaseWindow: await settle(bridge.releaseWindow()),
    showWindow: await settle(
      bridge.showWindow("0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f"),
    ),
    getLoginItem: await settle(bridge.getLoginItem()),
    setLoginItem: await settle(bridge.setLoginItem(true)),
    revealData: await settle(bridge.revealData()),
    openExternal: await settle(bridge.openExternal("https://example.com/")),
  };
  const invalid = {
    openExternal: await settle(bridge.openExternal(42)),
    copy: await settle(bridge.copy("x".repeat(1_000_001))),
    showWindow: await settle(bridge.showWindow("nope")),
    setLoginItem: await settle(bridge.setLoginItem("yes")),
    bootstrap: await settle(bridge.bootstrap("x")),
  };
  const forged = forge("getLoginItem");
  const afterForgery = await settle(bridge.releaseWindow());
  await report("calls", { calls, invalid, forged, afterForgery });
  const choosing = settle(bridge.chooseProject());
  await report("choosing");
  await report("chose", await choosing);
  const opened = window.open("https://example.com/popup");
  const link = Object.assign(document.createElement("a"), {
    href: "/download",
    download: "harness.bin",
  });
  document.body.append(link);
  link.click();
  location.href = "https://example.com/";
  await sleep(1500);
  await report("stayed", { href: location.href, opened: opened === null });
}

// The draft guard, as useKeepDraft installs it.
async function guard() {
  let vetoes = 0;
  const veto = (event) => {
    event.preventDefault();
    event.returnValue = true;
    vetoes += 1;
    report("vetoed", { vetoes });
  };
  addEventListener("beforeunload", veto);
  await report("guarded");
  for (;;) {
    const command = await next();
    if (command === "release")
      await report("released", await settle(bridge.releaseWindow()));
    if (command === "finish") {
      removeEventListener("beforeunload", veto);
      await bridge.releaseWindow();
    }
  }
}

async function probe() {
  const frame = new Promise((resolve) =>
    addEventListener("message", (event) => resolve(event.data), { once: true }),
  );
  const iframe = Object.assign(document.createElement("iframe"), {
    src: "/sandboxed",
  });
  iframe.sandbox = "allow-scripts";
  document.body.append(iframe);
  await report("frame", await frame);
  location.href = "/other";
  await sleep(2000);
  await report("stayed-after-redirect", { href: location.href });
}

async function sandboxed() {
  let parentBridge;
  try {
    parentBridge = typeof window.parent.nonstopvibin;
  } catch {
    parentBridge = "blocked";
  }
  const forged = forge("getLoginItem");
  window.parent.postMessage(
    {
      bridge: typeof window.nonstopvibin,
      parentBridge,
      forged,
      origin: self.origin,
    },
    "*",
  );
}

async function stray() {
  const forged = forge("getLoginItem");
  await report("stray", {
    origin: location.origin,
    bridge: typeof window.nonstopvibin,
    forged,
  });
}

const run =
  window.parent !== window
    ? sandboxed
    : location.hostname === "localhost"
      ? stray
      : new URLSearchParams(location.search).get("view") === "tray"
        ? () => report("tray", { bridge: typeof bridge })
        : profile === "00000000-0000-0000-0000-000000000000"
          ? probe
          : profile
            ? async () => {
                await report("profile", {
                  profile,
                  token: (await bridge.bootstrap()).token,
                });
                await guard();
              }
            : main;
run().catch((error) =>
  report("failed", { error: String(error), stack: error.stack }),
);
