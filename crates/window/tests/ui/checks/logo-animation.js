// The sidebar logo (fixtures/logo.tsx renders AnimatedLogo in the desktop brand area).
run(async () => {
  equal(
    matchMedia("(prefers-reduced-motion: no-preference)").matches,
    true,
    "the display allows motion",
  );
  const button = await present("button");
  const active = () => button.getAnimations({ subtree: true });
  const initial = active().length;
  const spin = active().find(
    (animation) => animation.animationName === "nv-spin",
  );
  spin.currentTime = 1200;
  const midpoint = getComputedStyle(
    button.querySelector(".nv-ball-wrap"),
    "::before",
  ).backgroundPosition;
  const transform = getComputedStyle(
    button.querySelector(".nv-ball"),
  ).transform;
  await Promise.all(active().map((animation) => animation.finished));
  const idle = active().length;
  button.click();
  await sleep(30);
  const replay = active().length;
  button.click();
  await sleep(30);
  const rapidReplay = active().length;
  await Promise.all(active().map((animation) => animation.finished));
  const settled = active().length;
  equal(midpoint, "50% 0px", "facets advance halfway around the Y axis");
  equal(transform, "none", "the round silhouette does not rotate or flatten");
  equal(initial, 4, "the logo plays its four animations");
  equal(replay, 4, "a click replays them");
  equal(rapidReplay, 4, "a second click restarts them");
  equal(idle, 0, "the first run ends");
  equal(settled, 0, "the replay ends");
  // A real click inside the brand area, which is the window's drag region.
  await click(".nv-logo-button");
  await sleep(30);
  equal(
    active().length,
    4,
    "the logo must receive mouse clicks inside the desktop drag region",
  );
  await Promise.all(active().map((animation) => animation.finished));

  // WebKit has no layout counter: nothing animates and nothing in the page changes while the
  // settled logo is on screen, so nothing asks for layout.
  await new Promise((done) =>
    requestAnimationFrame(() => requestAnimationFrame(done)),
  );
  let changes = 0;
  const observer = new MutationObserver(
    (records) => (changes += records.length),
  );
  observer.observe(document, {
    subtree: true,
    attributes: true,
    childList: true,
    characterData: true,
  });
  await sleep(300);
  observer.disconnect();
  equal(
    { animations: document.getAnimations().length, changes },
    { animations: 0, changes: 0 },
    "the settled logo does no layout work",
  );

  button.focus();
  await key(" ");
  await sleep(30);
  equal(document.getAnimations().length, 4, "Space replays the logo");
  emulateMedia("prefers-reduced-motion", "reduce");
  button.click();
  const reduced = document.getAnimations().length;
  equal(reduced, 0, "reduced motion plays nothing");
  return {
    initial,
    idle,
    replay,
    rapidReplay,
    settled,
    midpoint,
    transform,
    reduced,
  };
});
