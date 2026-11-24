// src/client/focus.ts and the built app's focus styles, on fixture markup that carries the
// components' class names (fixtures/focus.html). It does not check that the components
// render those classes; page-layout renders the real pages.
run(async () => {
  const shadow = () => getComputedStyle(document.activeElement).boxShadow;
  $("#first").focus();
  equal(outline(), "none", "activation/autofocus does not paint a button ring");
  await key("Tab");
  equal(
    document.activeElement.className,
    "tray-profile",
    "Tab moves to the profile row",
  );
  equal(outline(), "solid", "Tab shows the row focus indicator");
  equal(shadow(), "none", "only one ring");
  equal(
    getComputedStyle(document.activeElement).outlineOffset,
    "-3px",
    "tray ring stays inside the scroll area",
  );
  await key("Enter");
  equal($("details").open, true, "keyboard disclosure still works");
  for (const selector of [
    ".tray-account-summary",
    ".agent-tabs button",
    ".activity-page summary",
  ]) {
    $(selector).focus();
    equal(outline(), "solid", `${selector} has keyboard focus`);
    equal(shadow(), "none", `${selector} has only one ring`);
  }
  window.dispatchEvent(new Event("blur"));
  equal(outline(), "none", "deactivation clears stale rings");
  await key("Tab");
  equal(outline(), "solid", "keyboard navigation resumes after activation");
  $("#first").focus();
  await click("#first");
  equal(outline(), "none", "mouse use clears even existing keyboard rings");
  $(".tray-profile").focus();
  equal(
    outline(),
    "none",
    "focus restoration after a mouse action stays quiet",
  );
  $("input[type=checkbox]").focus();
  equal(outline(), "none", "switches stay quiet after mouse use");
  $("input").focus();
  equal(outline(), "solid", "text fields retain an editing indicator");
  await key("Tab");
  equal(
    document.activeElement.className,
    "select-item",
    "Tab moves to the option",
  );
  equal(
    outline(),
    "none",
    "select options use their highlighted row instead of a ring",
  );
  emulateMedia("forced-colors", "active");
  $("#first").focus();
  equal(outline(), "solid", "keyboard outline survives forced colors");
});
