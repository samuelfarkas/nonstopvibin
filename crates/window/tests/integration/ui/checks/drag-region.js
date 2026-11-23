// The macOS main window has no title bar: a press in a drag region (styles.css `--app-region`)
// moves it through the bridge script's `dragWindow`, and a button inside the region stays a
// button. The logo fixture nests the brand area as App.tsx does.
run(async () => {
  const button = await present(".nv-logo-button");
  const brand = $(".brand").getBoundingClientRect();
  const logo = button.getBoundingClientRect();
  ok(brand.right - logo.right > 20, "the brand area has room beside the logo");
  const y = logo.y + logo.height / 2;
  const delta = (moved) =>
    moved.after.map((value, i) => value - moved.before[i]);
  const beside = (logo.right + brand.right) / 2;
  const moved = await act({ drag: [beside, y, beside - 50, y + 30] });
  equal(delta(moved), [-50, 30], "a drag beside the logo moves the window");
  const centre = logo.x + logo.width / 2;
  const pressed = await act({ drag: [centre, y, centre - 50, y + 30] });
  equal(
    delta(pressed),
    [0, 0],
    "a drag from the logo button leaves the window",
  );
  return { moved, pressed };
});
