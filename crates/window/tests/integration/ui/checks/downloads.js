// A file the page makes, a blob: URL of its own origin as Activity's CSV export has, is saved in
// the Downloads folder; a file from the server and a data: URL are not.
const csv = "day,cost\n2026-11-24,1.5\n";
const save = (href, download) => {
  const anchor = Object.assign(document.createElement("a"), { href, download });
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
};
run(async () => {
  // As Activity's export: a click makes the file and revokes its URL a second later.
  const button = Object.assign(document.createElement("button"), {
    id: "export",
    textContent: "Export",
  });
  button.style.cssText = "position: fixed; top: 200px; left: 200px";
  button.addEventListener("click", () => {
    const url = URL.createObjectURL(new Blob([csv], { type: "text/csv" }));
    save(url, "activity.csv");
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });
  document.body.append(button);
  // A click straight after the first load can miss: the Linux menu bar is not in place yet.
  await act({ settle: 500 });
  await click("#export");
  await until(
    async () =>
      (await act({ downloads: true })).downloads["activity.csv"] === csv,
    "the export is saved in Downloads",
  );
  save("/focus.html", "page.html");
  await until(
    async () => (await act({ downloads: true })).refused === 1,
    "the download handler refuses the server's file",
  );
  // Linux stops it earlier, at the navigation check.
  save("data:text/csv,refused", "refused.csv");
  await act({ settle: 1000 });
  equal(
    (await act({ downloads: true })).downloads,
    { "activity.csv": csv },
    "only the export is saved",
  );
});
