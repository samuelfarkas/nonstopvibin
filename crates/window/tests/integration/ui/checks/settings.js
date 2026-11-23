// The second opening (desktop-smoke's cycle 1): the settings page edits one profile and one
// global draft at a time, and two drafts defer closing until both are gone.
run(async () => {
  await bootstrap();
  await logoRests();
  await press('[title="Settings"]');
  await present("#usage-retention");
  await retention(30);
  const created = await api("/profiles", "POST", { name: "Settings fixture" });
  equal(created.status, 201, "the second profile is created");
  const settingsProfile = created.body;
  await present('[title="Settings fixture"]');
  const sidebarProfile = $(".profiles [aria-current]").title;
  await present("#settings-profile");
  await choose("#settings-profile", (text) =>
    text.includes("Settings fixture"),
  );
  await until(
    () => $("#profile-name")?.value === "Settings fixture",
    "the settings page edits the second profile",
  );
  equal(
    $(".profiles [aria-current]").title,
    sidebarProfile,
    "settings picker leaves sidebar profile unchanged",
  );
  equal(
    $("#usage-retention").textContent,
    "30 days",
    "switching settings profile preserves global draft",
  );
  await press('[aria-label="Keep sessions on the same account"]');
  const profiles = async () => (await api("/state")).body;
  await until(
    async () =>
      (await profiles()).profiles.find((item) => item.id === settingsProfile.id)
        .sessionAffinity === !settingsProfile.sessionAffinity,
    "the second profile's setting is saved",
  );
  const current = await profiles();
  equal(
    current.profiles.find((item) => item.name === sidebarProfile)
      .sessionAffinity,
    true,
    "editing another profile leaves the original profile unchanged",
  );
  equal(
    current.usageRetentionDays,
    0,
    "global retention draft was not applied",
  );

  await closeGuarded(1, "unsaved settings are retained");
  await act({ show: true });
  equal(
    $("#usage-retention").textContent,
    "30 days",
    "the reopened window keeps the draft",
  );
  equal(
    await released(() => retention(0)),
    { asked: false, visible: true },
    "reopening cancels deferred close",
  );
  // One cleared guard must not discard another form in the same window.
  await retention(30);
  await press('[aria-label="New profile"]');
  await closeGuarded(2, "settings and dialog drafts defer the close");
  await progress("remaining dialog keeps its draft");
  const after = await released(() => retention(0));
  equal(after.asked, true, "a cleared guard releases the hidden window");
  equal(after.visible, false, "remaining dialog keeps its draft");
  // Discarding the last draft lets the pending close finish.
  return {
    then: "closes",
    after: () => $('[aria-label="Close dialog"]').click(),
  };
});
