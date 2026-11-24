// The third opening (desktop-smoke's cycle 2): the discarded retention draft never applied.
run(async () => {
  await bootstrap();
  await logoRests();
  await present("#usage-retention");
  equal(
    $("#usage-retention").textContent,
    "Keep all history",
    "settings page restores without applying the discarded limit",
  );
  return { then: "close" };
});
