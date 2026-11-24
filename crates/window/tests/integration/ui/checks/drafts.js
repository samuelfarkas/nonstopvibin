// The first opening of the main window (desktop-smoke's cycle 0): a pending operation and an
// unfinished form each defer closing, and a failure that arrives while hidden stays readable.
run(async () => {
  await bootstrap();
  equal(
    typeof Notification === "undefined"
      ? "denied"
      : await Notification.requestPermission(),
    "denied",
    "permission requests are denied",
  );
  const { body: profile } = await api("/profiles", "POST", {
    name: "Desktop fixture",
  });
  await present(".proxy-card button");
  // The start stays pending until the check lets it fail.
  let finish;
  const original = window.fetch;
  window.fetch = (url, options) => {
    if (!String(url).endsWith("/start")) return original(url, options);
    return new Promise((resolve) => {
      finish = () => {
        window.fetch = original;
        const error = { error: { message: "Fixture operation failed" } };
        resolve(new Response(JSON.stringify(error), { status: 503 }));
      };
    });
  };
  $(".proxy-card button").click();
  await closeGuarded(1, "a pending operation defers the close");
  finish();
  await until(
    () => $(".global-alert")?.textContent.includes("Fixture operation failed"),
    "the failure is shown",
  );
  // The window closing now ends the check with this label.
  await progress("background failure remains available to read");
  equal(
    (await act({ settle: 100 })).visible,
    false,
    "background failure remains available to read",
  );
  await act({ show: true });
  equal(
    await released(() => press('[aria-label="Dismiss error"]')),
    { asked: false, visible: true },
    "reopening cancels deferred close",
  );
  await api(`/profiles/${profile.id}/start`, "POST");
  // The real modal's guard, then discarded through its close button.
  await press('[aria-label="New profile"]');
  await closeGuarded(1, "unfinished form is retained");
  await act({ show: true });
  equal(
    await released(() => press('[aria-label="Close dialog"]')),
    { asked: false, visible: true },
    "reopening cancels deferred close",
  );
  await logoRests();
  // No draft is left: the close button closes the window.
  return { then: "close" };
});
