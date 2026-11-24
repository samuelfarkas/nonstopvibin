// The close button, then the daemon's Show while the page is still deciding whether the window
// may close: its late answer must not close or hide the window that is on screen again.
const slow = () => {
  const end = Date.now() + 4000;
  while (Date.now() < end);
};
run(async () => {
  addEventListener("beforeunload", slow);
  equal((await act({ closeThenShow: true })).visible, true, "shown again");
  equal(
    (await act({ settle: 1000 })).visible,
    true,
    "the late answer leaves the window open",
  );
  removeEventListener("beforeunload", slow);
});
