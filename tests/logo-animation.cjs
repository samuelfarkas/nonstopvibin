// Run with `bun tests/logo-animation.cjs` (isolated Electron renderer).
const assert = require("node:assert/strict");
const { writeFileSync, readFileSync } = require("node:fs");
const { join, resolve } = require("node:path");
const electron = require("electron");
const { checkPage, launchPage } = require("./support/electron-page.cjs");

if (typeof electron === "string") {
  launchPage("logo", __filename, (directory) => {
    require("esbuild").buildSync({
      stdin: {
        contents: `import React from 'react';
        import {createRoot} from 'react-dom/client';
        import {AnimatedLogo} from './src/client/components.tsx';
        createRoot(document.getElementById('root')).render(<AnimatedLogo state="running" />);`,
        resolveDir: resolve(__dirname, ".."),
        loader: "tsx",
      },
      bundle: true,
      outfile: join(directory, "logo.js"),
      define: { "process.env.NODE_ENV": '"production"' },
    });
    writeFileSync(
      join(directory, "index.html"),
      `<style>${readFileSync(resolve(__dirname, "../src/client/styles.css"), "utf8").replace("/mirrorball-spin.png", "data:image/png;base64," + readFileSync(resolve(__dirname, "../public/mirrorball-spin.png")).toString("base64"))}</style><div class="desktop"><div class="brand"><div id="root"></div></div></div><script src="logo.js"></script>`,
    );
  });
} else {
  checkPage(
    "Logo check",
    {
      show: true,
      webPreferences: { backgroundThrottling: false },
    },
    15000,
    async (window, directory) => {
      await window.loadFile(join(directory, "index.html"));
      await window.webContents.debugger.attach("1.3");
      await window.webContents.debugger.sendCommand(
        "Emulation.setEmulatedMedia",
        {
          features: [
            { name: "prefers-reduced-motion", value: "no-preference" },
          ],
        },
      );
      await window.loadFile(join(directory, "index.html"));
      const result = await window.webContents.executeJavaScript(`(async () => {
      const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
      while (!document.querySelector('button')) await delay(10);
      const button = document.querySelector('button');
      const active = () => button.getAnimations({subtree: true});
      const initial = active().length;
      const spin = active().find(animation => animation.animationName === 'nv-spin');
      spin.currentTime = 1200;
      const surface = getComputedStyle(button.querySelector('.nv-ball-wrap'), '::before');
      const midpoint = surface.backgroundPosition;
      const transform = getComputedStyle(button.querySelector('.nv-ball')).transform;
      await Promise.all(active().map(animation => animation.finished));
      const idle = active().length;
      button.click();
      await delay(30);
      const replay = active().length;
      button.click();
      await delay(30);
      const rapidReplay = active().length;
      await Promise.all(active().map(animation => animation.finished));
      return {initial, idle, replay, rapidReplay, settled: active().length, midpoint, transform};
    })()`);
      assert.equal(
        await window.webContents.executeJavaScript(
          "getComputedStyle(document.querySelector('button')).getPropertyValue('-webkit-app-region')",
        ),
        "no-drag",
        "the logo must receive mouse clicks inside the desktop drag region",
      );
      assert.equal(
        result.midpoint,
        "50% 0px",
        "facets advance halfway around the Y axis",
      );
      assert.equal(
        result.transform,
        "none",
        "the round silhouette does not rotate or flatten",
      );
      assert.equal(result.initial, 4);
      assert.equal(result.replay, 4);
      assert.equal(result.rapidReplay, 4);
      assert.equal(result.idle, 0);
      assert.equal(result.settled, 0);
      await window.webContents.debugger.sendCommand("Performance.enable");
      const layouts = async () =>
        (
          await window.webContents.debugger.sendCommand(
            "Performance.getMetrics",
          )
        ).metrics.find(({ name }) => name === "LayoutCount").value;
      // The frame after the animations end restores their properties. Then hidden, so no
      // window manager resize or pointer over the window adds layout work of its own.
      await window.webContents.executeJavaScript(
        "new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)))",
      );
      window.hide();
      const before = await layouts();
      await new Promise((resolve) => setTimeout(resolve, 300));
      assert.equal(
        (await layouts()) - before,
        0,
        "the settled logo does no layout work",
      );
      window.showInactive();
      await window.webContents.debugger.sendCommand(
        "Emulation.setFocusEmulationEnabled",
        { enabled: true },
      );
      await window.webContents.executeJavaScript(
        "document.querySelector('button').focus()",
      );
      await window.webContents.debugger.sendCommand("Input.dispatchKeyEvent", {
        type: "keyDown",
        key: " ",
        code: "Space",
        windowsVirtualKeyCode: 32,
      });
      await window.webContents.debugger.sendCommand("Input.dispatchKeyEvent", {
        type: "keyUp",
        key: " ",
        code: "Space",
        windowsVirtualKeyCode: 32,
      });
      assert.equal(
        await window.webContents.executeJavaScript(
          "document.getAnimations().length",
        ),
        4,
      );
      await window.webContents.debugger.sendCommand(
        "Emulation.setEmulatedMedia",
        {
          features: [{ name: "prefers-reduced-motion", value: "reduce" }],
        },
      );
      await window.webContents.executeJavaScript(
        "document.querySelector('button').click()",
      );
      const reduced = await window.webContents.executeJavaScript(
        "document.getAnimations().length",
      );
      assert.equal(reduced, 0);
      console.log("Logo animation passed:", { ...result, reduced });
    },
  );
}
