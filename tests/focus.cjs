// Run with `bun tests/focus.cjs` after `bun run build` (isolated Electron renderer).
// Checks src/client/focus.ts and the built app's focus styles on fixture markup that carries
// the components' class names. It does not check that the components render those classes;
// tests/page-layout.cjs renders the real pages.
const assert = require("node:assert/strict");
const { readdirSync, writeFileSync, readFileSync } = require("node:fs");
const { join, resolve } = require("node:path");
const electron = require("electron");
const { checkPage, launchPage } = require("./support/electron-page.cjs");

if (typeof electron === "string") {
  launchPage("focus", __filename, (directory) => {
    require("esbuild").buildSync({
      entryPoints: [resolve(__dirname, "../src/client/focus.ts")],
      bundle: true,
      format: "esm",
      outfile: join(directory, "focus.js"),
    });
    // Every stylesheet the build emits: the shared one first, then the main window's and the tray's.
    const assets = resolve(__dirname, "../dist/client/assets");
    const css = readdirSync(assets)
      .filter((name) => name.endsWith(".css"))
      .sort(
        (a, b) =>
          Number(b.startsWith("index-")) - Number(a.startsWith("index-")),
      )
      .map((name) => readFileSync(join(assets, name), "utf8"))
      .join("\n");
    writeFileSync(
      join(directory, "index.html"),
      `<style>${css}</style>
    <button id="first">Refresh</button>
    <details><summary class="tray-profile">Profile</summary>Quota</details>
    <details><summary class="tray-account-summary">Account</summary>Quota</details>
    <div class="agent-tabs"><button>Agent</button></div>
    <div class="activity-page"><details><summary>Activity</summary></details></div>
    <input aria-label="Profile name">
    <div class="select-item" tabindex="0" data-highlighted>Option</div>
    <input type="checkbox" aria-label="Enabled">
    <script type="module" src="focus.js"></script>`,
    );
  });
} else {
  checkPage(
    "Focus check",
    { show: false },
    15000,
    async (window, directory) => {
      await window.loadFile(join(directory, "index.html"));
      const contents = window.webContents;
      contents.debugger.attach("1.3");
      await contents.debugger.sendCommand(
        "Emulation.setFocusEmulationEnabled",
        { enabled: true },
      );
      const run = (script) => contents.executeJavaScript(script);
      const outline = () =>
        run("getComputedStyle(document.activeElement).outlineStyle");
      const key = async (value, code) => {
        await contents.debugger.sendCommand("Input.dispatchKeyEvent", {
          type: "keyDown",
          key: value,
          text: value === "Enter" ? "\r" : "",
          windowsVirtualKeyCode: code,
        });
        await contents.debugger.sendCommand("Input.dispatchKeyEvent", {
          type: "keyUp",
          key: value,
          windowsVirtualKeyCode: code,
        });
      };
      await run("document.querySelector('#first').focus()");
      assert.equal(
        await outline(),
        "none",
        "activation/autofocus does not paint a button ring",
      );
      await key("Tab", 9);
      assert.equal(
        await run("document.activeElement.className"),
        "tray-profile",
      );
      assert.equal(
        await outline(),
        "solid",
        "Tab shows the row focus indicator",
      );
      assert.equal(
        await run("getComputedStyle(document.activeElement).boxShadow"),
        "none",
        "only one ring",
      );
      assert.equal(
        await run("getComputedStyle(document.activeElement).outlineOffset"),
        "-3px",
        "tray ring stays inside the scroll area",
      );
      await key("Enter", 13);
      assert.equal(
        await run("document.querySelector('details').open"),
        true,
        "keyboard disclosure still works",
      );
      for (const selector of [
        ".tray-account-summary",
        ".agent-tabs button",
        ".activity-page summary",
      ]) {
        await run(`document.querySelector('${selector}').focus()`);
        assert.equal(
          await outline(),
          "solid",
          `${selector} has keyboard focus`,
        );
        assert.equal(
          await run("getComputedStyle(document.activeElement).boxShadow"),
          "none",
        );
      }
      await run("window.dispatchEvent(new Event('blur'))");
      assert.equal(await outline(), "none", "deactivation clears stale rings");
      await key("Tab", 9);
      assert.equal(
        await outline(),
        "solid",
        "keyboard navigation resumes after activation",
      );
      await run("document.querySelector('#first').focus()");
      const point = await run(
        "(() => { const r = document.querySelector('#first').getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; })()",
      );
      await contents.debugger.sendCommand("Input.dispatchMouseEvent", {
        type: "mousePressed",
        button: "left",
        clickCount: 1,
        ...point,
      });
      await contents.debugger.sendCommand("Input.dispatchMouseEvent", {
        type: "mouseReleased",
        button: "left",
        clickCount: 1,
        ...point,
      });
      assert.equal(
        await outline(),
        "none",
        "mouse use clears even existing keyboard rings",
      );
      await run("document.querySelector('.tray-profile').focus()");
      assert.equal(
        await outline(),
        "none",
        "focus restoration after a mouse action stays quiet",
      );
      await run("document.querySelector('input[type=checkbox]').focus()");
      assert.equal(
        await outline(),
        "none",
        "switches stay quiet after mouse use",
      );
      await run("document.querySelector('input').focus()");
      assert.equal(
        await outline(),
        "solid",
        "text fields retain an editing indicator",
      );
      await key("Tab", 9);
      assert.equal(
        await run("document.activeElement.className"),
        "select-item",
      );
      assert.equal(
        await outline(),
        "none",
        "select options use their highlighted row instead of a ring",
      );
      await contents.debugger.sendCommand("Emulation.setEmulatedMedia", {
        features: [{ name: "forced-colors", value: "active" }],
      });
      await run("document.querySelector('#first').focus()");
      assert.equal(
        await outline(),
        "solid",
        "keyboard outline survives forced colors",
      );
      console.log(
        "Focus checks passed: keyboard, mouse, restoration, text fields, disclosures, menus, forced colors.",
      );
    },
  );
}
