// The application menu the macOS helper installs (src/native/macos.rs, install_menu), used
// from the keyboard: Edit's shortcuts in a text field through the system pasteboard, View's
// accelerators, and Quit, which only asks the daemon.
run(async () => {
  const { bar, app, edit, view } = await act({ menus: true });
  equal(bar.slice(2), ["Edit", "View", "Window"], "the menu bar's menus");
  const has = (menu, items, label) =>
    equal(
      items.filter((item) => !menu.includes(item)),
      [],
      label,
    );
  has(app, ["Check for updates…", "Quit NonstopVibin"], "the app menu");
  has(edit, ["Undo", "Redo", "Cut", "Copy", "Paste", "Select All"], "Edit");
  has(
    view,
    ["Subscription quotas", "Open main window", "Actual Size", "Zoom In"],
    "View",
  );

  const field = $("input:not([type])");
  field.focus();
  await act({ type: "copy me" });
  await until(() => field.value === "copy me", "typing reaches the field");
  await act({ shortcut: "a" });
  await until(
    () => field.selectionStart === 0 && field.selectionEnd === 7,
    "⌘A selects the field's text",
  );
  await act({ shortcut: "c" });
  // WebKit writes the pasteboard after the menu action returns.
  await until(
    async () => (await act({ clipboard: true })).clipboard === "copy me",
    "⌘C copies the selection to the pasteboard",
  );
  await act({ shortcut: "x" });
  await until(() => field.value === "", "⌘X cuts the selection");
  await act({ shortcut: "v" });
  await until(() => field.value === "copy me", "⌘V pastes it back");

  await act({ sent: true });
  await act({ shortcut: "1" });
  equal(
    (await act({ sent: true })).sent,
    ["Command { command: OpenMain }"],
    "⌘1 asks the daemon for the main window",
  );
  await act({ shortcut: "u", shift: true });
  equal(
    (await act({ sent: true })).sent,
    ["Command { command: ToggleQuota }"],
    "⌘⇧U asks the daemon to toggle the quota popup",
  );
  const width = innerWidth;
  await act({ shortcut: "=" });
  await until(() => innerWidth < width, "⌘= zooms in");
  await act({ shortcut: "0" });
  await until(() => innerWidth === width, "⌘0 restores the actual size");
  await act({ shortcut: "q" });
  equal(
    (await act({ sent: true })).sent,
    ["Command { command: Quit }"],
    "⌘Q asks the daemon to quit; the helper keeps its window",
  );
  return { app, edit, view };
});
