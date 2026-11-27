# Provider support

| Service                       | Connection                                                             | Quota adapter                                                 | Verification in this build                                                             |
| ----------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| ChatGPT / Codex               | CLIProxyAPI OAuth; existing auth JSON                                  | Actual duration labels, primary/secondary/code-review windows | Core sign-in session lifecycle tested; live account inference and quota not yet tested |
| Claude Agent                  | Your own signed-in Claude Code (`claude`)                              | None yet; a reached limit answers 429                         | Engine tested with a fake `claude` and the real one against a local fake upstream      |
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

Subscription support follows the bundled [CLIProxyAPI core](https://github.com/router-for-me/CLIProxyAPI) and its multi-account load balancing. A Claude subscription connects through [Claude Agent](#claude-agent). Anthropic API keys are a separate connection option.

OpenCode Go is a subscription accessed with an API key, rather than an OAuth sign-in. It publishes model-specific wire protocols; nonstopvibin prefers discovery metadata and handles known model families. [OpenCode Go documentation](https://opencode.ai/docs/go/).

## Claude Agent

**Sign in → Claude Agent** serves a profile's Claude subscription through your own
signed-in Claude Code. Claude Code makes every request to Anthropic itself, with its real
client identity and the entrypoint label `nonstopvibin`, and nonstopvibin never reads or
copies the Claude login. Pi, Codex and OpenCode reach it through the profile as they
reach any other subscription.

1. Install Claude Code, so `claude` is on `PATH`, in `~/.local/bin` or
   `~/.claude/local`, or in `/opt/homebrew/bin` or `/usr/local/bin`.
2. Run the sign-in command the app shows, once per profile:
   `CLAUDE_CONFIG_DIR=<data>/claude-agent/<profile id> claude auth login`. The profile
   gets its own Claude Code configuration, so your everyday `claude` login is untouched.
3. Choose **Connect**. The models are the ones Claude Code offers that sign-in.

A profile holds one Claude Agent subscription, so nothing rotates between Claude
accounts; a reached limit is a 429. Each conversation
runs in a `claude` process with no built-in tools, no settings files and no
`CLAUDE.md`. The harness's own tools reach it over a local MCP server. Up to three run
per profile, and an idle one stops after five minutes. A client that hangs up
interrupts its turn. How it works is in
[the decision record](decisions/claude-agent.md).

**Account risk.** Anthropic's [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance)
says that developers building products or services with the Agent SDK should use
API-key authentication, and that OAuth is meant for ordinary use of Claude Code and
Anthropic's native apps. In January 2026 an Anthropic staff post on X called
third-party harnesses that use Claude subscriptions a Terms of Service violation.
Claude Agent neither imitates Claude Code nor handles the Claude login, so it carries
less risk than a proxy that does, as OpenCode and T3 Code also concluded. It does not
make driving a subscription from other harnesses sanctioned.

**The Claude sign-in is gone.** Earlier builds signed in to Claude through the proxy core,
which sent other harnesses' requests dressed up as Claude Code. On its first launch this
build deletes those Claude sign-ins from every profile: their credential files and their
accounts. Connect Claude Agent instead. A build from before Claude Agent cannot read a
store that holds a Claude Agent account, so going back to one is not supported.

Not verified yet: traffic of a real subscription, a real plan-limit 429 and its reset
time, and macOS. On Linux a `claude` process dies with the daemon; on macOS it exits
once its turn ends and its input closes.

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

## Resets made elsewhere

The proxy core keeps an account it saw at a limit out of routing until the reset
time the provider gave it, even after a reset on chatgpt.com or in this app. When a
quota check finds room in every window while the core still lists a quota cooldown for that account, the app clears it
through the core’s `POST /v0/management/reset-quota`. Quota is checked while the
app’s window is open and after a reset in the app, so open the app after a reset
made elsewhere. Cooldowns for other reasons, such as errors, are left alone.
