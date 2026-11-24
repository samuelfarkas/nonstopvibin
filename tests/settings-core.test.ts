// Settings' core section renders from application state alone. The daemon side (the state's
// coreVersion and the bundled pin) is covered by golden/api.json "GET /state" and
// crates/core/tests/integration/binary.rs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SettingsPage } from "../src/client/Settings.tsx";
import type { AppState, CoreState } from "../src/shared/types.ts";

function render(coreVersion: string, core: CoreState) {
  const state: AppState = {
    profiles: [],
    coreVersion,
    core,
    gateway: "",
    usageRetentionDays: 0,
    version: "0.2.0",
    errors: [],
  };
  return renderToStaticMarkup(
    createElement(SettingsPage, {
      state,
      refresh: async () => {},
      onError: assert.fail,
    }),
  );
}

test("Settings offers a reviewed core, then the bundled one, and shows progress", () => {
  const bundled = { bundled: "8.0.67", updates: true };
  const development = render("8.0.67", { ...bundled, updates: false });
  assert.match(development, /Installed builds update it here/);
  assert.doesNotMatch(development, /<button[^>]*>Update/);

  const offered = render("8.0.67", { ...bundled, available: "8.0.99" });
  assert.match(offered, /Version 8.0.99 is available/);
  assert.match(offered, />Update to 8.0.99<\/button>/);
  assert.doesNotMatch(offered, /Use bundled/);

  const installed = render("8.0.99", bundled);
  assert.match(installed, /Updated from the bundled 8.0.67/);
  assert.match(installed, />Use bundled 8.0.67<\/button>/);
  assert.match(installed, /<code>8.0.99<\/code>/);

  const busy = render("8.0.67", {
    ...bundled,
    available: "8.0.99",
    phase: "downloading",
  });
  assert.match(
    busy,
    /<button class="button" disabled="">.*Downloading…<\/button>/,
  );
  assert.doesNotMatch(busy, /Update to/);
});
