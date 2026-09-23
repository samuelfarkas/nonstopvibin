# Provider support

| Service                       | Connection                                                             | Quota adapter                                                 | Verification in this build                                                             |
| ----------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| ChatGPT / Codex               | CLIProxyAPI OAuth; existing auth JSON                                  | Actual duration labels, primary/secondary/code-review windows | Core sign-in session lifecycle tested; live account inference and quota not yet tested |
| Claude subscriptions          | CLIProxyAPI OAuth; existing auth JSON                                  | 5-hour, weekly and additional model windows                   | Parser and session plumbing tested; live account inference and quota not yet tested    |
| Anthropic API                 | API key                                                                | Subscription quota does not apply                             | Messages protocol translated through the real core to a local fixture                  |
| OpenCode Go                   | Subscription API key                                                   | Rolling, weekly, monthly                                      | Discovery and wire-protocol handling implemented; live Go key still needed             |
| Kimi Code                     | OAuth/device sign-in/import                                            | Usage and rate-limit windows                                  | Not shown in the app yet                                                               |
| Antigravity                   | OAuth/import                                                           | Per-model quota buckets                                       | Not shown in the app yet                                                               |
| Grok Build                    | OAuth/device sign-in/import                                            | Weekly credits and monthly billing windows                    | Not shown in the app yet                                                               |
| OpenAI / compatible providers | API key, URL, protocol, optional model prefix                          | Unavailable unless a specific adapter exists                  | Local integration fixtures exercise actual credential selection and streaming          |
| Gemini                        | Existing CLIProxyAPI auth-file import, or compatible API configuration | No dedicated adapter                                          | Import support only; no dedicated sign-in button or live test                          |

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

The pinned 7.2.151 core forwards redemption but predates upstream’s
[cooldown clearing fix](https://github.com/router-for-me/CLIProxyAPI/commit/80234b5).
If requests still fail with exhausted quota after redemption, stop and start the
profile. Synthetic tests cover the adapter and management boundaries; live credit
redemption has not been tested and no real credit was consumed during development.
