# Provider support

| Service                       | Connection                                                             | Quota adapter                                                 | Verification in this build                                                             |
| ----------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| ChatGPT / Codex               | CLIProxyAPI OAuth; existing auth JSON                                  | Actual duration labels, primary/secondary/code-review windows | Core sign-in session lifecycle tested; live account inference and quota not yet tested |
| Claude subscriptions          | CLIProxyAPI OAuth; existing auth JSON                                  | 5-hour, weekly and additional model windows                   | Parser and session plumbing tested; live account inference and quota not yet tested    |
| Anthropic API                 | API key                                                                | Subscription quota does not apply                             | Native Messages and cross-protocol paths through the real core to local fixtures       |
| OpenCode Go                   | Subscription API key                                                   | Rolling, weekly, monthly                                      | Discovery and wire-protocol handling implemented; live Go key still needed             |
| Kimi Code                     | OAuth/device sign-in/import                                            | Usage and rate-limit windows                                  | Not shown in the app yet                                                               |
| Antigravity                   | OAuth/import                                                           | Per-model quota buckets                                       | Not shown in the app yet                                                               |
| Grok Build                    | OAuth/device sign-in/import                                            | Weekly credits and monthly billing windows                    | Not shown in the app yet                                                               |
| OpenAI / compatible providers | API key, URL, protocol, optional model prefix                          | Unavailable unless a specific adapter exists                  | Local integration fixtures exercise actual credential selection and streaming          |
| Gemini                        | Existing CLIProxyAPI auth-file import, or compatible API configuration | No dedicated adapter                                          | Import support only; no dedicated sign-in button or live test                          |

Native Anthropic fixture checks also cover ordered tool SSE/replay, PNG bytes,
version/beta forwarding, provider error normalization, profile exhaustion and
idle transport cleanup after graceful FIN, TCP reset, Fetch abort and
ClientRequest destroy. They are not live-provider or native Claude-process
acceptance, nor proof of remote computation/billing cancellation. Custom-origin
`count_tokens` is a local text estimate that omits image tokens; a truncated or
failed native stream ends with an error event, not `message_stop`, and cools
down its account. See the
[protected-upstream limitations](server.md#native-anthropic-transport-and-limitations)
before admitting unattended consumers.

For custom routes, cross-protocol reasoning/thinking metadata comes from CLIProxyAPI's model catalog. A configured model ID absent from the core's remote catalog can therefore lose those controls during translation, even while routing remains configured.

Subscription support follows the bundled [CLIProxyAPI core](https://github.com/router-for-me/CLIProxyAPI), which supports Claude OAuth login and multi-account load balancing. Connect a Claude subscription through **Sign in → Claude**, or import an existing CLIProxyAPI auth file into its profile. Anthropic API keys are a separate connection option.

OpenCode Go is a subscription accessed with an API key, rather than an OAuth sign-in. It publishes model-specific wire protocols; nonstopvibin prefers discovery metadata and handles known model families. [OpenCode Go documentation](https://opencode.ai/docs/go/).

## Codex banked resets

Open a Codex subscription’s details to load banked resets automatically. Choose
**Review reset** on a credit, then confirm **Use 1 reset**. The app shows its scope and expiration,
redeems only that credit, then refreshes usage. API-key accounts do not support
subscription resets. Nothing is redeemed automatically.

This uses OpenAI’s ChatGPT backend through CLIProxyAPI’s authenticated management
`/api-call`, not the public OpenAI inference API. The official Codex client
implements [credit listing and consumption](https://github.com/openai/codex/blob/main/codex-rs/backend-client/src/client/rate_limit_resets.rs).
OpenAI documents [banked resets and eligibility](https://help.openai.com/en/articles/20001498).
The provider decides which usage windows can reset; a full reset can change the
weekly reset date.

Retries for the same account and credit reuse a persisted redemption ID, including
after an uncertain network response or app restart. Successful IDs remain saved;
definitive `nothing_to_reset` and `no_credit` responses permit a fresh attempt.

Synthetic tests cover the adapter and management boundaries; live credit
redemption has not been tested and no real credit was consumed during development.

## Claude usage resets

Open a Claude subscription’s details to load the limit resets Claude offers it:
the same ones claude.ai shows under **Settings → Usage**. The app lists one row per
kind of reset (Fable, full, 5-hour, weekly) with how many are left, which limits it
refills and when it expires. A kind Claude lets you use now offers **Review reset**;
confirm **Use 1 reset** to redeem its first usable grant, as Claude Code does, then
usage refreshes. A kind you cannot use yet says why, in Claude Code’s words, such as
“usable once you reach a limit it refills”. When Claude offers the subscription no
resets, the panel shows its reason. Nothing is redeemed automatically.

This follows the official Claude Code client (2.1.332, its `cedar_ember` program):
the grants come from `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`
and a claim is `POST https://api.anthropic.com/api/organizations/<organization>/reset_rate_limits`
with the grant and a request ID, sent through CLIProxyAPI’s authenticated management
`/api-call` with the seat’s own token. Neither endpoint is a documented public API.

**Client identity.** Claude lists and redeems resets only for Claude Code: any other
client gets `ineligible_reason: "surface"` and no grants, and an old Claude Code gets
`"cli_version"`. Like CLIProxyAPI’s own Management Center, the app therefore sends
these calls with the Claude Code User-Agent its proxy core already presents for every
Claude request, `claude-cli/2.1.321 (external, cli)` (`crates/core/src/upstream.rs`;
re-read it when the core pin moves). Anthropic’s
[terms](https://code.claude.com/docs/en/legal-and-compliance) reserve subscription
sign-in for Claude Code and Anthropic’s own apps and allow enforcement without
notice; using a reset this way carries the same risk to the account as the rest of
the app’s Claude support. Resets can always be used on claude.ai instead, and the app
then clears its cooldown as described below.

A retry after an uncertain answer reuses the request ID, including after an app
restart. A grant holds several resets, so once Claude names an outcome the next
claim uses a new ID. A seat without a verified organization and account must be
reconnected first. Claude’s at-limit-only `juniper_tide` offer is not supported.
Listing was checked live against two Claude seats; no real reset was redeemed during
development.

## Resets made elsewhere

The proxy core keeps an account it saw at a limit out of routing until the reset
time the provider gave it, even after a reset on claude.ai, chatgpt.com or in this
app. When a quota check finds room in every window, account-wide and per model,
while the core still lists a quota cooldown for that account, the app clears it
through the core’s `POST /v0/management/reset-quota`. Quota is checked while the
app’s window is open and after a reset in the app, so open the app after a reset
made elsewhere. Cooldowns for other reasons, such as errors, are left alone.
