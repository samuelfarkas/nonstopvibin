// Executed with the exact generated controls in a fresh VM by the test wrapper.
async (
  assert,
  connectProfileControls,
  directory,
  { createHash, mkdir, writeFile },
) => {
  let sessionId = 0;
  function session(
    saved = [],
    initial = "nonstopvibin-work",
    missing = [],
    workSlug = "work",
    initialModel = "shared",
  ) {
    const workProvider = "nonstopvibin-" + workSlug;
    const profiles = [
      { provider: workProvider, name: "Work", slug: workSlug },
      { provider: "nonstopvibin-personal", name: "Personal", slug: "personal" },
    ];
    const providers = new Map();
    const native = new Map();
    const commands = new Map();
    const handlers = new Map();
    const subscribers = new Map();
    const branch = [...saved];
    const requests = [];
    const notices = [];
    const menus = [];
    const choices = [];
    let current;
    let busy = false;
    let pending = false;
    let auth = true;
    let afterAuth;
    let footer;
    for (const id of new Set([
      ...profiles.map((item) => item.provider),
      "other",
      initial,
    ])) {
      const models = (
        missing.includes(id)
          ? []
          : [
              "shared",
              "gpt-5.6-sol",
              "gpt-5.6-luna",
              "claude-opus-5",
              id === workProvider ? "work-only" : "personal-only",
            ]
      ).map((name) => ({ provider: id, id: name }));
      const stream = (model, context, options) => {
        requests.push({ provider: id, model: model.id, context, options });
        return "synthetic-stream";
      };
      providers.set(id, {
        id,
        name: id,
        auth: { source: id },
        getModels: () => models,
        stream,
        streamSimple: stream,
        fetchDeferred: stream,
      });
      if (id === initial)
        current = models.find((model) => model.id === initialModel);
    }
    const builtins = new Map(providers);
    const ctx = {
      cwd: directory,
      hasUI: true,
      get model() {
        return current;
      },
      isIdle: () => !busy,
      hasPendingMessages: () => pending,
      sessionManager: { getBranch: () => branch },
      modelRegistry: {
        getAll: () =>
          [...providers.values()].flatMap((item) => item.getModels()),
        refresh: async () => {},
        getAvailable: () =>
          [...providers.values()].flatMap((item) => item.getModels()),
        getProvider: (id) => providers.get(id),
        getRegisteredNativeProvider: (id) => native.get(id),
        getRegisteredProviderConfig: () => undefined,
        find: (id, model) =>
          providers
            .get(id)
            ?.getModels()
            .find((item) => item.id === model),
      },
      ui: {
        setStatus: (_key, text) => {
          footer = text;
        },
        notify: (text) => notices.push(text),
        select: async (title, options) => {
          menus.push({ title, options });
          const choice = choices.shift();
          return typeof choice === "function" ? choice(options) : choice;
        },
      },
    };
    const emit = async (name, event = {}) => {
      let result;
      for (const handler of handlers.get(name) ?? [])
        result = await handler(event, ctx);
      return result;
    };
    const api = {
      exec: async () => ({ code: 1, stdout: "" }),
      events: {
        emit: (name, event) => {
          for (const handler of subscribers.get(name) ?? []) handler(event);
        },
        on: (name, handler) =>
          subscribers.set(name, [...(subscribers.get(name) ?? []), handler]),
      },
      on: (name, handler) =>
        handlers.set(name, [...(handlers.get(name) ?? []), handler]),
      registerCommand: (name, command) => {
        assert.equal(
          commands.has(name),
          false,
          "one command for multiple profile extensions",
        );
        commands.set(name, command);
      },
      registerProvider: (provider) => {
        providers.set(provider.id, provider);
        native.set(provider.id, provider);
      },
      unregisterProvider: (id) => {
        native.delete(id);
        providers.set(id, builtins.get(id));
      },
      appendEntry: (customType, data) =>
        branch.push({ type: "custom", customType, data }),
      setModel: async (model) => {
        if (!auth) return false;
        if (afterAuth) await afterAuth();
        const previousModel = current;
        current = model;
        branch.push({
          type: "model_change",
          provider: model.provider,
          modelId: model.id,
        });
        if (
          previousModel?.provider !== model.provider ||
          previousModel?.id !== model.id
        )
          await emit("model_select", { model, previousModel, source: "set" });
        return true;
      },
    };
    const preferences = directory + "/preferences-" + sessionId++;
    const preferenceFile =
      preferences +
      "/" +
      createHash("sha256").update(directory).digest("hex") +
      ".json";
    for (const profile of profiles)
      connectProfileControls(api, profile, preferences);
    return {
      ctx,
      api,
      branch,
      requests,
      notices,
      menus,
      choices,
      providers,
      emit,
      get footer() {
        return footer;
      },
      get model() {
        return current;
      },
      command: (args = "") => commands.get("nv").handler(args, ctx),
      setBusy: (value) => {
        busy = value;
      },
      setPending: (value) => {
        pending = value;
      },
      setAuth: (value) => {
        auth = value;
      },
      setAfterAuth: (fn) => {
        afterAuth = fn;
      },
      request: (id, method = "streamSimple") => {
        const provider = providers.get(id);
        return provider[method](
          { provider: id, id: "shared" },
          { messages: [] },
          { apiKey: "synthetic", marker: true },
        );
      },
      writePreference: async (provider, modelId) => {
        await mkdir(preferences, { recursive: true });
        await writeFile(preferenceFile, JSON.stringify({ provider, modelId }));
      },
    };
  }

  const cleanup = session();
  const nativeOther = cleanup.providers.get("other");
  cleanup.api.registerProvider(nativeOther);
  await cleanup.emit("session_start");
  await cleanup.emit("session_shutdown");
  await cleanup.emit("session_shutdown");
  assert.equal(
    cleanup.providers.get("other"),
    nativeOther,
    "restore the original native registration",
  );
  assert.equal(
    cleanup.request("nonstopvibin-personal"),
    "synthetic-stream",
    "builtin registrations lose the retired lock",
  );
  const replacement = session();
  await replacement.emit("session_start");
  replacement.api.registerProvider(nativeOther);
  await replacement.emit("session_shutdown");
  assert.equal(
    replacement.providers.get("other"),
    nativeOther,
    "cleanup preserves a later owner's registration",
  );

  const freshStart = { type: "session_start", reason: "startup" };
  for (const scenario of [
    {
      name: "explicit GPT model",
      preference: ["nonstopvibin-carvago", "gpt-5.6-sol"],
      initial: ["nonstopvibin-carvago", "gpt-5.6-luna"],
      expected: ["nonstopvibin-carvago", "gpt-5.6-luna"],
    },
    {
      name: "explicit Claude model",
      preference: ["nonstopvibin-carvago", "gpt-5.6-sol"],
      initial: ["nonstopvibin-carvago", "claude-opus-5"],
      expected: ["nonstopvibin-carvago", "claude-opus-5"],
    },
    {
      name: "repository fallback",
      preference: ["nonstopvibin-carvago", "gpt-5.6-sol"],
      initial: ["openai-codex", "gpt-5.6-sol"],
      expected: ["nonstopvibin-carvago", "gpt-5.6-sol"],
    },
    {
      name: "personal explicit model",
      preference: ["nonstopvibin-personal", "gpt-5.6-sol"],
      initial: ["nonstopvibin-personal", "claude-opus-5"],
      expected: ["nonstopvibin-personal", "claude-opus-5"],
    },
    {
      name: "explicit third-party profile",
      preference: ["nonstopvibin-carvago", "claude-opus-5"],
      initial: ["nonstopvibin-third", "claude-opus-5"],
      expected: ["nonstopvibin-third", "claude-opus-5"],
    },
    {
      name: "provider stability",
      preference: ["nonstopvibin-carvago", "claude-opus-5"],
      initial: ["nonstopvibin-personal", "claude-opus-5"],
      expected: ["nonstopvibin-personal", "claude-opus-5"],
    },
  ]) {
    const current = session(
      [],
      scenario.initial[0],
      [],
      "carvago",
      scenario.initial[1],
    );
    await current.writePreference(...scenario.preference);
    await current.emit("session_start", freshStart);
    assert.equal(current.model.provider, scenario.expected[0], scenario.name);
    assert.equal(current.model.id, scenario.expected[1], scenario.name);
    assert.deepEqual(
      current.branch.at(-1).data,
      { provider: scenario.expected[0], modelId: scenario.expected[1] },
      scenario.name + " records its chosen identity",
    );
  }
  const recorded = session(
    [
      {
        type: "custom",
        customType: "nonstopvibin-profile",
        data: {
          provider: "nonstopvibin-carvago",
          modelId: "gpt-5.6-luna",
        },
      },
    ],
    "nonstopvibin-personal",
    [],
    "carvago",
    "claude-opus-5",
  );
  await recorded.writePreference("nonstopvibin-carvago", "gpt-5.6-sol");
  await recorded.emit("session_start", freshStart);
  assert.equal(recorded.model.provider, "nonstopvibin-carvago");
  assert.equal(
    recorded.model.id,
    "gpt-5.6-luna",
    "existing conversation identity outranks launch and repository defaults",
  );

  const work = session();
  const personal = session([], "nonstopvibin-personal");
  await work.emit("session_start");
  await personal.emit("session_start");
  assert.match(work.footer, /Work/);
  assert.match(personal.footer, /Personal/);
  assert.equal(work.request("nonstopvibin-work"), "synthetic-stream");
  assert.equal(work.requests[0].options.marker, true);
  assert.equal(
    work.providers.get("nonstopvibin-work").auth.source,
    "nonstopvibin-work",
  );
  for (const method of ["stream", "streamSimple", "fetchDeferred"]) {
    assert.throws(
      () => work.request("nonstopvibin-personal", method),
      /Switch profiles with \/nv/,
    );
    assert.throws(
      () => work.request("other", method),
      /Switch profiles with \/nv/,
    );
  }
  assert.equal(
    work.requests.length,
    1,
    "foreign requests never reach the provider",
  );

  work.choices.push("work-only");
  await work.command("model");
  assert.equal(work.model.id, "work-only");
  assert.equal(
    work.menus.at(-1).options.some((label) => label.includes("personal")),
    false,
  );
  await work.api.setModel({ provider: "nonstopvibin-personal", id: "shared" });
  assert.equal(
    work.model.provider,
    "nonstopvibin-work",
    "native model selection cannot switch profile",
  );
  assert.equal(work.model.id, "work-only");

  await work.command("personal"); // No matching model; cancelling leaves Work intact.
  assert.equal(work.model.provider, "nonstopvibin-work");
  work.choices.push("shared");
  await work.command("personal");
  assert.equal(work.model.provider, "nonstopvibin-personal");
  assert.equal(work.request("nonstopvibin-personal"), "synthetic-stream");
  assert.throws(() => work.request("nonstopvibin-work"), /Switch profiles/);
  assert.match(personal.footer, /Personal/, "another session is independent");

  await work.command("work"); // Same model exists; no extra model question.
  assert.equal(work.model.provider, "nonstopvibin-work");
  const beforeFailure = JSON.stringify(work.branch.at(-1));
  work.setAuth(false);
  await assert.rejects(
    work.command("personal"),
    /authentication is unavailable/,
  );
  assert.equal(JSON.stringify(work.branch.at(-1)), beforeFailure);
  assert.match(work.footer, /Work/);
  work.setAuth(true);

  for (const condition of [work.setBusy, work.setPending]) {
    condition(true);
    await work.command("personal");
    assert.equal(work.model.provider, "nonstopvibin-work");
    condition(false);
  }
  work.choices.push(() => {
    work.setBusy(true);
    return "personal-only";
  });
  await work.command("model");
  assert.equal(
    work.model.id,
    "shared",
    "becoming busy during a dialog prevents a switch",
  );
  work.setBusy(false);
  work.setAfterAuth(async () => {
    work.setAfterAuth(undefined);
    assert.throws(() => work.request("nonstopvibin-work"), /model is changing/);
    assert.equal((await work.emit("input")).action, "handled");
    work.setBusy(true);
  });
  await work.command("personal");
  assert.equal(
    work.model.provider,
    "nonstopvibin-work",
    "auth delay cannot switch a now-busy conversation",
  );
  work.setBusy(false);

  const restored = session(work.branch, "other");
  await restored.emit("session_start");
  assert.equal(restored.model.provider, "nonstopvibin-work");
  assert.throws(() => restored.request("other"), /Switch profiles/);
  const unavailable = session(work.branch, "other", ["nonstopvibin-work"]);
  await unavailable.emit("session_start");
  assert.equal((await unavailable.emit("input")).action, "handled");
  assert.throws(() => unavailable.request("other"), /Switch profiles/);
  await unavailable.command("work");
  assert.match(unavailable.notices.at(-1), /No models/);
  assert.throws(
    () => unavailable.request("nonstopvibin-personal"),
    /Switch profiles/,
  );

  const legacy = session(
    [
      {
        type: "model_change",
        provider: "nonstopvibin-deleted",
        modelId: "shared",
      },
    ],
    "other",
  );
  await legacy.emit("session_start");
  assert.throws(
    () => legacy.request("other"),
    /Switch profiles/,
    "a missing legacy profile must not adopt pi's fallback",
  );
  for (const slug of ["a".repeat(39) + "-", "a".repeat(39) + "--2"]) {
    const provider = "nonstopvibin-" + slug;
    const original = session([], provider, [], slug);
    await original.emit("session_start");
    const resumed = session(original.branch, "other", [], slug);
    await resumed.emit("session_start");
    assert.equal(
      resumed.model.provider,
      provider,
      "existing truncated slugs survive resume",
    );
  }
  const malformed = session(
    [{ type: "custom", customType: "nonstopvibin-profile", data: {} }],
    "other",
  );
  await malformed.emit("session_start");
  assert.throws(() => malformed.request("other"), /Switch profiles/);

  work.choices.push((options) =>
    options.find((option) => option.startsWith("Use other pi providers")),
  );
  await work.command();
  assert.equal(work.request("other"), "synthetic-stream");
  const unlocked = session(work.branch, "other");
  await unlocked.emit("session_start");
  assert.equal(unlocked.request("other"), "synthetic-stream");
  work.branch.splice(0, work.branch.length, ...personal.branch);
  await work.emit("session_tree");
  assert.equal(work.model.provider, "nonstopvibin-personal");
  assert.throws(() => work.request("nonstopvibin-work"), /Switch profiles/);
};
