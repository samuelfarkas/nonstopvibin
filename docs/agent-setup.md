# Native agent setup

Start a profile, open **Connect agents**, choose **Claude Code**, **Codex**, or
**pi**, then click **Connect**. Keep the app and profile running while you work. The profile endpoint and key are under
**Advanced connection details**; normal setup never copies a key or asks you to run a launcher.

| Agent       | Connect once                                                                                                                                 | Daily use                                                                                                                 |
| ----------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| Claude Code | Choose the main project folder; merge connection fields into `.claude/settings.local.json` and write a profile-specific picker settings file | Run the generated `claude --settings …` command in that project. Choose with `/model`; verify the gateway with `/status`. |
| Codex       | Add `~/.codex/nonstopvibin-<slug>.config.toml`                                                                                               | Run `codex --profile nonstopvibin-<slug>` in any project, then choose an available model with `/model` before prompting.  |
| pi          | Add an automatically loaded `.js` provider extension under `~/.pi/agent/extensions/`                                                         | Run `pi`, choose a profile once with `/nv`, then use `/model`. The footer shows the conversation's locked profile.        |

Restart an already-running agent after setup. Pi also supports `/reload` for
extensions. Setup uses supported environment overrides for the agent config
location when they are available to the app process. A desktop launched outside
your shell may not inherit those overrides; inspect **Connection details** for
the actual paths before using an alternate config directory.

Install the clients using their official instructions: [Claude Code](https://code.claude.com/docs/en/setup),
[Codex](https://learn.chatgpt.com/docs/cli), and [pi](https://pi.dev/). Setup does
not install or upgrade clients. Credential commands need `curl` on PATH. Verified
macOS clients: Claude Code **2.1.243** (full custom picker; requires the generated
`--settings` command), Codex **0.140.0**, and pi **0.85.1**. Older clients may lack
these interfaces. A newer version still needs verification when upstream changes.

## Scope and existing settings

Codex gets a separate profile file. Pi gets a separate extension file. Its main
config, comments, stored auth, other providers, and defaults are left intact. Pi extension filenames are `nonstopvibin-<slug>.js`, matching their provider IDs.
Delete that file and `/reload` to remove a profile from pi, or use **Disconnect**
in the app to also remove its credential helper. Reconnect migrates the old UUID
filename only when its contents still match the owned installation. In pi, use `/nv` to explicitly change or release the
conversation's profile lock before choosing another provider.

Claude uses local project settings because global settings changes can affect
running sessions. Use **Connect another project** to connect the same profile to more repositories;
the project selector manages each connection separately. Choose the repository root, not a linked
worktree: Claude shares local settings with the main checkout. Close that
project's Claude sessions before changing or removing its connection. App profile
selection alone does not change any agent settings.

Claude's existing unrelated settings are merged and retained. Existing conflicting
connection fields are refused, not backed up in plaintext or silently replaced.
Generated files and credential helpers must be ordinary files owned by your user;
symlink descendants, special files and shared write access are rejected. Disconnect
also refuses conflicting external edits. Manually resolve those fields before
retrying. Setup does not change trust decisions, approval rules, or sandbox policy.

## Keys, endpoint changes, and disconnect

The app stores profile keys using its existing encrypted storage. Generated files
contain only non-secret connection metadata and native hooks. Claude, Codex, and
pi credential helpers read the selected profile's key from an owner-only Unix
socket. They never request management credentials. Helper and
manifest files live under `<app data>/agent-connections/<slug>/`.

Each connection keeps a fixed profile URL. If the gateway port changes, old
helpers refuse to return credentials. Reconnect the agent and restart its session.
Updating a helper does not redirect it to a different profile. Stopping a profile
fails requests inside that profile; the gateway never borrows another profile's
accounts.

**Disconnect** removes only this connection's settings and app-owned files.
Restart the agent afterwards. Native clients can cache keys; disconnect is not
revocation. Stop the profile or quit the app to stop access immediately. Legacy
launcher files from older builds are not deleted automatically; stop using them
once connected natively. Profiles are not an isolation boundary against programs
running as your OS user.

## Model selection and checks

### pi: choose the profile separately

Reconnect each existing pi connection once to install the profile controls, then
restart pi or `/reload`. Older connections are marked **Reconnect**. All connected
profiles contribute to one `/nv` command; disconnecting one leaves the others usable.

- `/nv` chooses a profile and remembers it for this repository. `/nv <slug>` selects
  it directly. The profile picker supports search, arrow keys, Enter and Escape.
- Native `/model` lists the active profile's models without copies from the other
  profiles. Native model changes are remembered too; `/nv model` remains a compatibility
  alias. Pi's manually configured favorite-model scope remains separate: use the
  picker's **all** scope if old favorites refer to another profile.
- Opening pi or using `/new` restores the repository's last selected profile and
  model when pi starts from another provider. An explicitly launched
  `nonstopvibin-*` provider/model stays authoritative. Subdirectories share the Git
  checkout's preference. A worktree without its own preference inherits the main
  checkout's profile and model. An explicit profile/model change (including releasing
  the lock) saves a worktree-only override; inheritance itself creates no extra file.
  Existing conversations keep their own selection. Outside Git, preferences are per
  working directory.
  Preferences live in the app's private agent data (`agents/pi-preferences/`), never
  in tracked repository files, and contain only profile/model IDs. Concurrent
  sessions keep their own choices; the last explicit change wins for future sessions.
- Switching profiles keeps the model when the destination offers the same ID;
  otherwise it asks for a model. Cancelling or failed authentication preserves the
  current profile. Switches are refused during a turn, retry, compaction, or queued work.
- The footer shows the active profile. Resume, reload and branch navigation restore
  the conversation's own selection ahead of the repository preference. A missing
  profile/model blocks requests instead of adopting pi's fallback. Start the profile
  and `/reload`, or explicitly choose another with `/nv`.
- `/nv` → **Use other pi providers** releases the lock and remembers that preference.
  Native providers return to `/model`; inactive nonstopvibin profiles stay hidden.

The integration uses pi's [extension lifecycle and provider API](https://pi.dev/docs/latest/extensions)
and its [provider catalog interface](https://github.com/earendil-works/pi/blob/main/packages/ai/src/models.ts).
It does not replace the built-in `/model` command or rewrite pi's startup settings.

A profile switch continues the existing conversation, including its context, through
the chosen profile. Start a new pi session when that context should remain separate.
Each open pi session has its own selection; changing the app's selected profile does
not redirect any of them. Repository preferences also apply in print/RPC mode when
the host does not launch an explicit nonstopvibin model; the interactive profile
picker is intended for the TUI. Existing conversation selections always take priority.

The design keeps existing provider IDs, endpoints, credential helpers and pi's
protocol implementations. A provider stream guard enforces the selected profile;
request-notification hooks are unsuitable because pi catches their errors. The
scope-only alternative is bypassable through pi's all-model view, and pi 0.85.1
exposes no public extension method to replace the native scope. A mutable shared
provider alias would complicate restore and authentication identity. Revisit native
picker integration if pi adds a supported scope/filter API. These controls cover
this pi runtime, not independent subagents or arbitrary third-party extensions.
[Pi extension API](https://raw.githubusercontent.com/earendil-works/pi/v0.85.1/packages/coding-agent/src/core/extensions/types.ts),
[request hook error handling](https://raw.githubusercontent.com/earendil-works/pi/v0.85.1/packages/coding-agent/src/core/extensions/runner.ts).

### Catalogs

Setup connects the full live profile catalog. Select models inside the agent; connecting does not set a foreground, review, or subagent model. Codex can retain its built-in or saved default even when it is unavailable in this profile; choose an available model with `/model` before your first prompt, or pass its native `--model` option in noninteractive use. Native Codex model/reasoning preferences are retained on reconnect and disconnect. Reconnect existing single-model installations once to remove their owned overrides. User-owned model preferences remain intact.

Claude Code gets every live profile ID, including GPT/Codex models, through its native `modelPicker` in a separate app-owned `--settings` file. Project/local settings ignore that field, so use the generated command. **Sync models** updates this snapshot after subscriptions or models change; restart Claude afterwards. Plain `claude` still loads the project's route but does not load the complete custom picker. Codex uses CLIProxyAPI's native `models?client_version=...` catalog, including Claude models and the core's client compatibility metadata. Pi fetches its full profile catalog on startup and refreshes on `/reload`. Availability always comes from the running profile's core, so models in another profile or only in a public catalog are never added.

The core refreshes its upstream subscription model catalog at startup and every three hours. New model IDs can appear without a core binary update once the upstream catalog includes them. Refresh **Available models** in Connect, then use pi's `/reload` to load the updated list.

[models.dev](https://models.dev) supplies pi capabilities, context/input/output limits, and USD prices per million tokens. Matches require the exact routing provider and model ID, with configured account prefixes removed only for metadata lookup. There are no guessed model families, version aliases, token budgets or zero-price placeholders. Metadata is fetched without credentials over HTTPS, validated and cached in memory for up to one hour; a failed refresh does not silently use an expired cache. The management models endpoint supports `?metadata=1` to inspect source URLs and fetch timestamps.

Pi and OpenCode install the live models with verified text/tool capabilities, limits, and input/output prices. Models lacking that metadata are omitted and listed beside the connection; setup fails when no usable models remain. A refresh failure also surfaces at agent startup. Unknown custom routes need an exact provider entry in models.dev; same-named models at unrelated vendors cannot establish their price. No real accounts are contacted to guess missing data.

Prices are API list estimates, not subscription bills. Missing cache rates are omitted and labelled unavailable (native clients can still display their own zero defaults). Pi receives published context-price tiers. Claude Code and Codex retain their native cost reporting; their discovery schemas do not offer the same custom pricing interface. Catalog capabilities describe upstream models and do not prove every translated tool, reasoning, or image feature works through CLIProxyAPI.

**Check profile** verifies the key and nonempty profile catalog through the gateway's catalog.
It sends no inference request. **Configuration saved** means files were installed,
not that a real agent session or subscription has been tested.

- **Claude Code:** verify `/status` before prompting. Managed policy or Claude apps
  gateway login may take precedence. The full picker requires 2.1.243+ and the generated command. Native Default/active rows can still appear; choose a live profile ID. Optional gateway discovery filters to Claude/Anthropic IDs and needs 2.1.257+ with nonessential traffic disabled. Disabling nonessential traffic is not an egress firewall.
- **Codex:** native command-backed auth deliberately omits conflicting `env_key`,
  inline token, and `requires_openai_auth` fields. The pinned core serves the native Codex catalog format and all current profile IDs. Built-in and cached models can also appear; an entry is not proof of current profile availability. Use **Available models** in Connect to check live IDs. Desktop/IDE profile selection is not verified.
- **pi:** a same-provider stored key or explicit CLI key may supersede the helper.
  Namespacing avoids ordinary built-in-provider collisions. Existing commented
  `models.json` is never rewritten by native setup.

Cross-provider use is experimental. CLIProxyAPI owns translation; no model aliases or second protocol adapter are added. Anthropic does not officially support non-Claude models in Claude Code. Provider-specific tools, context limits, reasoning choices and model-switch signature behavior can differ.

## Verification

The profile controls were checked with pi 0.85.1's actual extension
loader and SDK against a synthetic loopback server. Run the same probe with
`node scripts/check-pi-profiles.mjs /path/to/@earendil-works/pi-coding-agent`.
It strictly type-checks the generated controls against the installed pi APIs and
checks OpenAI-compatible and Anthropic streaming, wrong-profile refusal, native
model selection/cycling, catalog refresh, reload/unlock, concurrent sessions and
restored unavailable profiles. Its settings and credentials are disposable; no real accounts
or global pi settings are used. Repository tests cover cancellation, authentication
failure, busy/queued work, malformed saved state, existing slug formats, provider
cleanup ownership and old-connection migration too.

Synthetic native-client checks exercised Claude project settings/helper auth,
Codex native profile/command auth and Responses streaming, and pi extension
discovery with request-time credential rotation. They used isolated temporary configuration
and no real accounts. These checks do not certify every tool, subagent, translated
model, or Linux desktop behavior.

Repository tests also exercise both directions through CLIProxyAPI 7.2.151 with synthetic Responses/Anthropic providers: fragmented tool arguments, two calls, tool-result replay, reasoning effort and transport-shape-valid synthetic signatures, and foreign-profile rejection. These are protocol checks, not proof of provider-issued signature validity or every model.

Repository tests cover native config generation, exact owned-field removal,
conflicts, comment preservation, missing/invalid inputs, shell/TOML/JavaScript
escaping, symlink/special-file refusal, private socket authentication, key rotation,
port drift, restart, stopped profiles, management isolation, and real-core gateway
routing/streaming. These checks do not replace live provider or native Linux desktop
testing.
