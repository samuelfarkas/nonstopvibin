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

Open a Claude subscription’s details to load the usage resets Claude offers it.
The app lists each reset grant with resets left, which limits it refills and when
it expires; only the grant Claude would redeem next, while it is usable now, offers
**Review reset**. Confirm **Use 1 reset** to redeem one, then usage refreshes.
Nothing is redeemed automatically, and subscriptions outside the program list none.

This follows the official Claude Code client (2.1.292, its `cedar_ember` program):
the grants come from `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`
and a claim is `POST https://api.anthropic.com/api/organizations/<organization>/reset_rate_limits`
with the grant and a request ID, sent through CLIProxyAPI’s authenticated management
`/api-call` with the seat’s own token. Neither endpoint is a documented public API,
so Claude can change or refuse it; the app does not imitate Claude Code’s client
headers, and reports the outcome Claude names. A seat without a verified organization and
account must be reconnected first.

A retry after an uncertain answer reuses the request ID, including after an app
restart. A grant holds several resets, so once Claude names an outcome the next
claim uses a new ID. Claude’s at-limit-only `juniper_tide` offer is not supported;
live claims have not been tested and no real reset was used during development.

## Resets made elsewhere

The proxy core keeps an account it saw at a limit out of routing until the reset
time the provider gave it, even after a reset on claude.ai, chatgpt.com or in this
app. When a quota check finds room in every window, account-wide and per model,
while the core still lists a quota cooldown for that account, the app clears it
through the core’s `POST /v0/management/reset-quota`. Quota is checked while the
app’s window is open and after a reset in the app, so open the app after a reset
made elsewhere. Cooldowns for other reasons, such as errors, are left alone.
