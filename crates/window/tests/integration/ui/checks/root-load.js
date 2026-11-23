// The built entry with its main and tray root chunks refused (site.rs answers them 404).
run(async () => {
  const view =
    new URLSearchParams(location.search).get("view") === "tray"
      ? "tray"
      : "main";
  for (let i = 0; i < 250 && !$(".fatal button"); i++) await sleep(20);
  match(
    $(".fatal")?.textContent ?? "",
    /Reload nonstopvibin/,
    `the ${view} window shows the recovery view`,
  );
  equal(uncaught, [], "no unhandled rejection");
});
