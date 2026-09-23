// Included in each owned extension. pi's session-scoped event bus elects one
// command owner, so connecting/removing profiles needs no shared-file installer.
export const piProfileControls = String.raw`
import { mkdir, readFile, rename, writeFile, rm, realpath } from "node:fs/promises";
import { join } from "node:path";
import { createHash, randomUUID } from "node:crypto";
/** @typedef {import("@earendil-works/pi-coding-agent").ExtensionContext} Context */
/** @typedef {NonNullable<ReturnType<Context["modelRegistry"]["getProvider"]>>} Provider */
/** @typedef {{provider: string, name: string, slug: string}} Profile */
/** @param {string} text */
const printable = (text) => text.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, " ");
/** @param {Context} ctx @param {string} title @param {string[]} options @returns {Promise<string | undefined>} */
async function chooseProfileOption(ctx, title, options) {
  // RPC exposes select dialogs but cannot render custom terminal components.
  if (ctx.mode !== "tui") return ctx.ui.select(title, options);
  const { Container, Input, SelectList, Text, fuzzyFilter } = await import("@earendil-works/pi-tui");
  return ctx.ui.custom((tui, theme, keys, done) => {
    const container = new Container();
    const input = new Input({ placeholder: "Search…" });
    const results = new Container();
    const items = options.map((value) => ({ value, label: printable(value) }));
    /** @type {import("@earendil-works/pi-tui").SelectList} */
    let list;
    const filter = () => {
      list = new SelectList(fuzzyFilter(items, input.getValue(), (item) => item.label), 10, {
        selectedPrefix: (text) => theme.fg("accent", text),
        selectedText: (text) => theme.fg("accent", text),
        description: (text) => theme.fg("muted", text),
        scrollInfo: (text) => theme.fg("dim", text),
        noMatch: () => theme.fg("warning", "No matches"),
      });
      list.onSelect = (item) => done(item.value);
      list.onCancel = () => done(undefined);
      results.clear();
      results.addChild(list);
    };
    container.addChild(new Text(theme.fg("accent", printable(title)), 0, 1));
    container.addChild(input);
    container.addChild(results);
    container.addChild(new Text(theme.fg("dim", "Type to search · ↑↓ navigate · enter select · esc cancel"), 0, 1));
    filter();
    return {
      get focused() { return input.focused; },
      set focused(value) { input.focused = value; },
      render: (width) => container.render(width),
      invalidate: () => container.invalidate(),
      handleInput(data) {
        if ((/** @type {const} */ (["tui.select.up", "tui.select.down", "tui.select.confirm", "tui.select.cancel"])).some((key) => keys.matches(data, key))) list.handleInput(data);
        else { input.handleInput(data); filter(); }
        tui.requestRender();
      },
    };
  });
}
/** @param {import("@earendil-works/pi-coding-agent").ExtensionAPI} pi @param {Profile} profile @param {string} preferencesDirectory */
function connectProfileControls(pi, profile, preferencesDirectory) {
  profile = { ...profile, name: printable(profile.name) };
  const channel = "nonstopvibin:profiles:v1";
  /** @type {{ profiles?: Map<string, Profile> }} */
  const shared = {};
  pi.events.emit(channel, shared);
  if (shared.profiles) {
    shared.profiles.set(profile.provider, profile);
    return;
  }
  const profiles = new Map([[profile.provider, profile]]);
  pi.events.on(channel, (request) => { if (request && typeof request === "object") Object.assign(request, { profiles }); });
  const entryType = "nonstopvibin-profile";
  /** @type {Map<string, {wrapped: Provider, models: Provider["getModels"], restore: () => void}>} */
  const guarded = new Map();
  /** @type {string | null} */
  let active = null;
  /** @type {string | null} */
  let modelId = null;
  let changing = false;
  let picking = false;
  const label = () => profiles.get(active ?? "")?.name ?? active;
  const save = () => pi.appendEntry(entryType, { provider: active, modelId });
  let preferencePath = "";
  /** @param {Context} ctx */
  async function remember(ctx) {
    save();
    const temporary = preferencePath + "." + randomUUID();
    try {
      await mkdir(preferencesDirectory, { recursive: true, mode: 0o700 });
      await writeFile(temporary, JSON.stringify({ provider: active, modelId }), { mode: 0o600, flag: "wx" });
      await rename(temporary, preferencePath);
    } catch {
      notify(ctx, "Could not save the repository profile preference. This conversation still remembers it.");
    } finally {
      await rm(temporary, { force: true });
    }
  }
  /** @param {Context} ctx */
  const updateVisibility = (ctx) => ctx.modelRegistry.refresh({ allowNetwork: false });
  /** @param {Context} ctx */
  const status = (ctx) => ctx.ui.setStatus("nonstopvibin", active
    ? "nonstopvibin · " + label() + " · /model"
    : "nonstopvibin · /nv to choose a profile");
  /** @param {Context} ctx @param {string} message */
  const notify = (ctx, message) => ctx.ui.notify(message, "warning");

  /** @param {Context} ctx @param {Iterable<string>} ids */
  function protect(ctx, ids) {
    for (const id of ids) {
      if (guarded.has(id) && ctx.modelRegistry.getRegisteredNativeProvider(id) === guarded.get(id)?.wrapped) continue;
      const originalNative = ctx.modelRegistry.getRegisteredNativeProvider(id);
      const originalConfig = ctx.modelRegistry.getRegisteredProviderConfig(id);
      const provider = ctx.modelRegistry.getProvider(id);
      if (!provider) continue;
      const check = () => {
        if (changing) throw new Error("The nonstopvibin model is changing. Try again when the switch finishes.");
        if (active && id !== active) throw new Error("This conversation uses nonstopvibin · " + label() + ". Switch profiles with /nv.");
        if (active && !ctx.modelRegistry.find(active, modelId ?? "")) throw new Error("The saved nonstopvibin model is unavailable. Choose an available model with /model or /nv.");
      };
      // Notification hooks swallow errors. Guard the actual transport instead,
      // preserving the original provider's auth, protocols and request options.
      /** @type {Provider} */
      const wrapped = {
        ...provider,
        // Pi's credential updates rebuild snapshots without filterModels. Hide at
        // the catalog source so /model stays scoped during those updates too.
        getModels() {
          return (active ? id === active : !profiles.has(id)) ? provider.getModels() : [];
        },
        stream(model, context, options) { check(); return provider.stream(model, context, options); },
        streamSimple(model, context, options) { check(); return provider.streamSimple(model, context, options); },

      };
      const fetchDeferred = provider.fetchDeferred;
      if (fetchDeferred) wrapped.fetchDeferred = (model, handle, options) => { check(); return fetchDeferred.call(provider, model, handle, options); };
      pi.registerProvider(wrapped);
      guarded.set(id, { wrapped, models: () => provider.getModels(), restore: () => {
        if (originalNative) pi.registerProvider(originalNative);
        else {
          pi.unregisterProvider(id);
          if (originalConfig) pi.registerProvider(id, originalConfig);
        }
      } });
    }
  }

  // Pi keeps ModelRuntime across /reload. Do not leave guards with dead session state.
  pi.on("session_shutdown", (_event, ctx) => {
    for (const [id, registration] of guarded) {
      if (ctx.modelRegistry.getRegisteredNativeProvider(id) === registration.wrapped) {
        if (profiles.has(id)) pi.unregisterProvider(id);
        else registration.restore();
      }
    }
    guarded.clear();
  });

  /** @param {Context} ctx @param {NonNullable<Context["model"]>} model */
  async function selectModel(ctx, model) {
    changing = true;
    try {
      if (!await pi.setModel(model)) throw new Error("Profile authentication is unavailable. Start it in nonstopvibin and /reload.");
      modelId = model.id;
    } finally {
      changing = false;
    }
    status(ctx);
  }

  /** @param {import("@earendil-works/pi-coding-agent").SessionStartEvent | import("@earendil-works/pi-coding-agent").SessionTreeEvent} event @param {Context} ctx */
  async function restore(event, ctx) {
    const cwd = await realpath(ctx.cwd);
    let repository = cwd;
    let mainRepository = cwd;
    try {
      const git = await pi.exec("git", ["-C", cwd, "rev-parse", "--show-toplevel"], { timeout: 3000 });
      if (git.code === 0) {
        repository = await realpath(git.stdout.replace(/\n$/, ""));
        mainRepository = repository;
        // Git lists the main checkout first; NUL delimiters preserve unusual paths.
        const worktrees = await pi.exec("git", ["-C", repository, "worktree", "list", "--porcelain", "-z"], { timeout: 3000 });
        const first = worktrees.stdout.split("\0")[0];
        if (worktrees.code === 0 && first.startsWith("worktree ")) mainRepository = await realpath(first.slice(9));
      }
    } catch {
      mainRepository = repository; // Git is optional; keep the local preference if discovery fails.
    }
    const preferencePaths = [...new Set([repository, mainRepository])].map((path) => join(preferencesDirectory, createHash("sha256").update(path).digest("hex") + ".json"));
    preferencePath = preferencePaths[0];
    const branch = ctx.sessionManager.getBranch();
    const saved = branch.findLast((entry) => entry.type === "custom" && entry.customType === entryType);
    let data = saved?.type === "custom" ? saved.data : undefined;
    // Existing conversations keep their recorded identity. An explicitly launched
    // nonstopvibin model outranks repository defaults.
    if (!saved && !ctx.model?.provider.startsWith("nonstopvibin-") && event.type === "session_start" && (event.reason === "startup" || event.reason === "new") && !branch.some((entry) => entry.type === "message")) {
      for (const path of preferencePaths) {
        try { data = JSON.parse(await readFile(path, "utf8")); break; }
        catch (error) {
          if (error && typeof error === "object" && "code" in error && error.code === "ENOENT") continue;
          data = {}; // Corrupt overrides must not fall back to a different profile.
          break;
        }
      }
    }
    if (saved?.type === "custom" || data !== undefined) {
      if (!data || typeof data !== "object" || !("provider" in data) || !("modelId" in data) || !(data.provider === null || (typeof data.provider === "string" && /^nonstopvibin-[a-z0-9][a-z0-9-]*$/.test(data.provider))) || !(data.modelId === null || typeof data.modelId === "string")) {
        active = "nonstopvibin-unavailable";
        modelId = null;
        notify(ctx, "The saved nonstopvibin profile is invalid. Choose it again with /nv.");
      } else {
        active = data.provider;
        modelId = data.modelId;
      }
    } else {
      // On legacy sessions use their recorded identity, not pi's fallback when
      // the original provider disappeared. New sessions may use a native default.
      const recorded = branch.findLast((entry) => entry.type === "model_change");
      const provider = recorded?.provider ?? ctx.model?.provider ?? (profiles.size === 1 ? profile.provider : undefined);
      active = provider?.startsWith("nonstopvibin-") ? provider : null;
      modelId = active ? (recorded?.modelId ?? ctx.model?.id ?? null) : null;
    }
    // Include unauthenticated providers: /login can make them available without reloading.
    protect(ctx, new Set([...ctx.modelRegistry.getAll().map((model) => model.provider), ...profiles.keys(), ctx.model?.provider ?? ""]));
    await updateVisibility(ctx);
    if (active) {
      const model = ctx.modelRegistry.find(active, modelId ?? "") ?? (modelId === null ? ctx.modelRegistry.getAvailable().find((item) => item.provider === active) : undefined);
      if (!model) notify(ctx, "The saved model in " + label() + " is unavailable. Start the profile and /reload, or choose with /nv.");
      else if (ctx.model?.provider !== active || ctx.model?.id !== modelId) await selectModel(ctx, model);
    }
    if (!saved) save();
    status(ctx);
  }
  pi.on("session_start", restore);
  pi.on("session_tree", restore);

  pi.on("model_select", async (event, ctx) => {
    protect(ctx, [event.model.provider]);
    if (changing) return;
    if (active && event.model.provider !== active) {
      const previous = ctx.modelRegistry.find(active, modelId ?? "");
      if (previous) await selectModel(ctx, previous);
      notify(ctx, "This conversation stays in " + label() + ". Use /model for its models, or /nv to switch profiles.");
    } else {
      if (!active && profiles.has(event.model.provider)) {
        active = event.model.provider;
        await updateVisibility(ctx);
      }
      if (active) { modelId = event.model.id; await remember(ctx); }
    }
    status(ctx);
  });

  pi.on("input", (_event, ctx) => {
    protect(ctx, new Set([...ctx.modelRegistry.getAvailable().map((model) => model.provider), ctx.model?.provider ?? ""]));
    if (changing) { notify(ctx, "The nonstopvibin model is changing. Try again when the switch finishes."); return { action: "handled" }; }
    if (active && (ctx.model?.provider !== active || !ctx.modelRegistry.find(active, modelId ?? "") || !ctx.modelRegistry.find(active, ctx.model?.id ?? ""))) {
      notify(ctx, "Choose an available model for " + label() + " with /model, or switch profiles with /nv.");
      return { action: "handled" };
    }
    return { action: "continue" };
  });

  pi.registerCommand("nv", {
    description: "Choose and remember the nonstopvibin profile for this repository",
    getArgumentCompletions: (prefix) => [...profiles.values()].map((item) => item.slug)
      .filter((value) => value.startsWith(prefix)).map((value) => ({ value, label: value })),
    handler: async (args, ctx) => {
      if (!ctx.hasUI) throw new Error("Use /nv in interactive pi.");
      if (picking || !ctx.isIdle() || ctx.hasPendingMessages()) {
        notify(ctx, "Wait for this turn and queued messages to finish before changing profiles or models.");
        return;
      }
      picking = true;
      try {
        const argument = args.trim();
        /** @type {string | null | undefined} */
        let target = active;
        if (argument !== "model") {
          if (argument) {
            target = [...profiles.values()].find((item) => item.slug === argument)?.provider;
            if (!target) { notify(ctx, "Unknown profile. Use /nv to choose a connected profile."); return; }
          } else {
            const choices = [...profiles.values()].sort((a, b) => Number(b.provider === active) - Number(a.provider === active) || a.name.localeCompare(b.name));
            const labels = choices.map((item) => item.name + " (" + item.slug + ")" + (item.provider === active ? " · active" : ""));
            const other = "Use other pi providers · release profile lock";
            const choice = await chooseProfileOption(ctx, "Profile for this repository · existing context continues", [...labels, other]);
            if (choice === undefined) return;
            if (choice === other) {
              if (!ctx.isIdle() || ctx.hasPendingMessages()) { notify(ctx, "The conversation became busy. Try /nv when it finishes."); return; }
              active = null;
              modelId = null;
              await updateVisibility(ctx);
              await remember(ctx);
              status(ctx);
              ctx.ui.notify("Profile lock released. Choose another provider with /model.", "info");
              return;
            }
            target = choices[labels.indexOf(choice)]?.provider;
          }
        }
        if (!target) { notify(ctx, "Choose a profile with /nv first."); return; }
        const models = [...(guarded.get(target)?.models() ?? [])];
        if (!models.length) { notify(ctx, "No models are available for this profile. Start it in nonstopvibin and /reload."); return; }
        let model = argument !== "model" && target !== active ? models.find((item) => item.id === ctx.model?.id) : undefined;
        if (!model) {
          models.sort((a, b) => Number(b.id === ctx.model?.id) - Number(a.id === ctx.model?.id) || a.id.localeCompare(b.id));
          const labels = models.map((item) => item.id + (item.id === ctx.model?.id && target === active ? " · current" : ""));
          const choice = await chooseProfileOption(ctx, "Model · " + (profiles.get(target)?.name ?? target), labels);
          if (choice === undefined) return;
          model = models[labels.indexOf(choice)];
        }
        if (!model) return;
        if (!ctx.isIdle() || ctx.hasPendingMessages()) { notify(ctx, "The conversation became busy. Try /nv when it finishes."); return; }
        protect(ctx, [target]);
        // Auth failure leaves the existing lock untouched. No request is made by setModel.
        const previousModel = ctx.model;
        const previousId = modelId;
        await selectModel(ctx, model);
        if (!ctx.isIdle() || ctx.hasPendingMessages()) {
          if (previousModel) await selectModel(ctx, previousModel);
          modelId = previousId;
          notify(ctx, "The conversation became busy. Profile switching was cancelled.");
          return;
        }
        active = target;
        await updateVisibility(ctx);
        await remember(ctx);
        status(ctx);
        ctx.ui.notify("nonstopvibin · " + label() + " · " + printable(model.id), "info");
      } finally {
        picking = false;
      }
    },
  });
}
`;
