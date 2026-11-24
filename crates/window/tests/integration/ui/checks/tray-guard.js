// The built app with a /state that answers once and then fails, polled quickly.
let stateCalls = 0;
const state = {
  profiles: [
    {
      id: "p",
      slug: "work",
      name: "Work",
      strategy: "round-robin",
      sessionAffinity: false,
      enabled: true,
      createdAt: "",
      runtime: "running",
      accounts: [],
      endpoint: "",
    },
  ],
  coreVersion: "",
  core: { bundled: "", updates: false },
  gateway: "",
  usageRetentionDays: 0,
  version: "",
  errors: [],
};
{
  const interval = window.setInterval;
  window.setInterval = (callback, ms, ...rest) =>
    interval(callback, Math.min(ms, 50), ...rest);
}
window.fetch = async (url) => {
  if (new URL(url, location.href).pathname !== "/api/state")
    return new Response("{}", { status: 404 });
  return ++stateCalls === 1
    ? new Response(JSON.stringify(state), { status: 200 })
    : new Response(
        JSON.stringify({ error: { message: "Fixture poll failed" } }),
        { status: 503 },
      );
};
run(async () => {
  await until(() => stateCalls >= 4, "the app polls /state four times");
  await sleep(50);
  return { guards: guards() };
});
