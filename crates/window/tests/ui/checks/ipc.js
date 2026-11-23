// What a page other than the bridge script can post to the helper's IPC channel, and what
// reaches the daemon's end (`act({ bridge: true })`: calls forwarded, messages dropped).
const post = (message) => {
  try {
    window.webkit.messageHandlers.ipc.postMessage(message);
    return "posted";
  } catch (error) {
    return String(error);
  }
};
// The bridge script's message shape, without its nonce.
const forge = (method) =>
  post(JSON.stringify({ nonce: "forged", id: 1, method }));

if (window.top !== window) {
  let parentBridge;
  try {
    parentBridge = typeof window.parent.nonstopvibin;
  } catch {
    parentBridge = "blocked";
  }
  const seen = {
    origin: location.origin,
    bridge: typeof window.nonstopvibin,
    parentBridge,
  };
  window.parent.postMessage({ ...seen, forged: forge("getLoginItem") }, "*");
}

const observed = () => act({ bridge: true });
// What the framed page saw, or null when it never ran (the helper refused to load it).
function framed(src) {
  const answer = new Promise((resolve) =>
    addEventListener("message", (event) => resolve(event.data), { once: true }),
  );
  document.body.append(
    Object.assign(document.createElement("iframe"), { src }),
  );
  return Promise.race([answer, sleep(3000).then(() => null)]);
}
const dropped = (reason) => (count) => Array(count).fill(reason);
const unsigned = dropped("the message did not come from the bridge script");

run(async () => {
  const bridge = window.nonstopvibin;
  equal(
    await bridge.getLoginItem(),
    true,
    "control: the page's own call is answered",
  );
  equal(
    await observed(),
    { forwarded: ["GetLoginItem"], dropped: [] },
    "control: the page's own call reaches the daemon",
  );

  const same = await framed("/frame.html");
  equal(
    { origin: same.origin, bridge: same.bridge },
    { origin: location.origin, bridge: "undefined" },
    "a subframe on the daemon origin gets no bridge of its own",
  );
  equal(
    await observed(),
    { forwarded: [], dropped: unsigned(same.forged === "posted" ? 1 : 0) },
    "a subframe on the daemon origin gets no bridge reply",
  );

  // WebKit may refuse to load it at all; if it runs, it has no bridge.
  const off = await framed(`${config.elsewhere}/frame.html`);
  if (off)
    equal(
      {
        origin: off.origin,
        bridge: off.bridge,
        parentBridge: off.parentBridge,
      },
      {
        origin: config.elsewhere,
        bridge: "undefined",
        parentBridge: "blocked",
      },
      "an off-origin page gets no bridge",
    );
  const offSeen = await observed();
  equal(
    { forwarded: offSeen.forwarded, dropped: offSeen.dropped.length },
    { forwarded: [], dropped: off?.forged === "posted" ? 1 : 0 },
    "an off-origin page gets no bridge reply",
  );

  equal(
    [post("not json"), post("[1]")],
    ["posted", "posted"],
    "the page can post anything",
  );
  equal(
    await observed(),
    { forwarded: [], dropped: dropped("the message is not a JSON object")(2) },
    "a malformed message is refused",
  );

  post("x".repeat(6 * 1_000_000 + 1025));
  equal(
    await observed(),
    { forwarded: [], dropped: ["the message is too long"] },
    "an oversized message is refused",
  );

  equal(
    typeof bridge.deleteEverything,
    "undefined",
    "the bridge offers no unknown method",
  );
  forge("deleteEverything");
  equal(
    await observed(),
    { forwarded: [], dropped: unsigned(1) },
    "an unknown method is refused",
  );

  equal(
    await bridge.getLoginItem(),
    true,
    "control: the bridge still answers afterwards",
  );
  return { offOriginFrame: off ? "ran" : "refused" };
});
