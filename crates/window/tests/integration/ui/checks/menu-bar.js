// The menu bar the Linux helper gives the main window (src/native/linux.rs, menu.rs), used from
// the keyboard: View's shortcuts ask the daemon for windows and zoom the page, Close asks the
// page as the close button does, and Quit only asks the daemon.
run(async () => {
  const sent = async () => (await act({ sent: true })).sent;
  await sent();
  await act({ shortcut: "1" });
  equal(
    await sent(),
    ["Command { command: OpenMain }"],
    "Ctrl+1 asks the daemon for the main window",
  );
  await act({ shortcut: "u", shift: true });
  equal(
    await sent(),
    ["Command { command: ToggleQuota }"],
    "Ctrl+Shift+U asks the daemon to toggle the quota popup",
  );

  const width = innerWidth;
  await act({ shortcut: "=" });
  await until(() => innerWidth < width, "Ctrl+= zooms in");
  await act({ shortcut: "0" });
  await until(() => innerWidth === width, "Ctrl+0 restores the actual size");
  await act({ shortcut: "-" });
  await until(() => innerWidth > width, "Ctrl+- zooms out");
  await act({ shortcut: "0" });
  await until(() => innerWidth === width, "Ctrl+0 restores it again");

  // A draft keeps the window: Close hides it instead.
  let asked = 0;
  const keep = (event) => {
    asked += 1;
    event.preventDefault();
  };
  window.addEventListener("beforeunload", keep);
  await act({ shortcut: "w" });
  await until(
    async () => !(await act({ settle: 100 })).visible,
    "Ctrl+W hides the window that keeps a draft",
  );
  equal(asked, 1, "Ctrl+W asks the page first");
  window.removeEventListener("beforeunload", keep);
  await act({ show: true });

  await act({ shortcut: "q" });
  equal(
    await sent(),
    ["Command { command: Quit }"],
    "Ctrl+Q asks the daemon to quit; the helper keeps its window",
  );
});
