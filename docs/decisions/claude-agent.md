# Decision record: serve Claude subscriptions through the user's own `claude`

Status: accepted for a first version, November 2026. The policy evidence comes from a
deep-research run with checked citations, kept outside the repository. The spike evidence
below was run on Hive against Claude Code 2.1.326 and CLIProxyAPI 8.1.0.

## Context

A profile's Claude subscription used to be a Claude OAuth credential in its CLIProxyAPI core.
For every harness that is not Claude Code, the core imitates Claude Code: it adds a billing
header, the "You are Claude Code" identity block, a made-up `metadata.user_id` and a pinned
device profile ("cloaking"). It sends that to Anthropic with the user's OAuth token. Anthropic's
legal and compliance page forbids third parties from routing requests through Claude.ai
credentials on behalf of their users. It also forbids them to "collect, store, or intermediate"
those credentials. It allows an end user signing in to the unmodified Claude Code binary with
their own subscription. OpenCode (`claude-agent`, Oct 2026) and T3 Code run the genuine binary
through the Agent SDK for this reason.

The same page says developers building products or services with the Agent SDK should use
API-key authentication, and that OAuth is meant for ordinary use of Claude Code and Anthropic's
native apps. In January 2026 an Anthropic staff post on X called third-party harnesses that use
Claude subscriptions a Terms of Service violation. This route removes the fingerprint imitation
and the credential intermediation, so it carries less risk than cloaking. It does not make
driving a subscription from other harnesses sanctioned.

## Decision

A **Claude Agent** subscription runs the user's own signed-in `claude` program. Claude Code makes
every request to Anthropic itself, with its real fingerprint and the honest entrypoint label
`nonstopvibin`. nonstopvibin never reads, copies or refreshes the Claude login.

```
harness ─(Chat | Responses | Messages)─▶ daemon gateway /p/<slug>
  ▶ profile core (translation, accounting): claude-api-key, base-url = engine, cloak never
  ▶ engine, in the daemon, loopback: /claude-agent/<profile>/v1/messages
      ▶ `claude -p` (stream-json over stdio), CLAUDE_CONFIG_DIR=<data>/claude-agent/<profile>
          ▶ Anthropic
      ◀ MCP over HTTP: /claude-agent/<profile>/mcp — the harness's own tools
```

### What changed from the proposal brief

- **No Agent SDK, no JavaScript runtime.** The daemon is Rust now. The SDK is a wrapper that runs
  `claude -p --input-format stream-json --output-format stream-json` and speaks the same JSON
  lines. The engine drives that documented headless CLI directly. It hands Claude the
  harness's tools as an HTTP MCP server on the daemon's loopback listener instead of the SDK's
  in-process server. This avoids a roughly 270 MB SDK download and a Node or Bun process.
- **The harness's system prompt replaces Claude Code's.** With `--system-prompt-file`, Claude
  Code sends only its billing header, the line "You are a Claude agent, built on Anthropic's
  Claude Agent SDK." and the harness's prompt. The brief expected the Claude Code preset with the
  harness's prompt appended.
- **Session identity does not come from headers.** CLIProxyAPI forwards none of the harness
  session headers (`x-session-affinity`, `session_id`, `x-opencode-session`) or
  `prompt_cache_key` to a `claude-api-key` base URL. It forwards a Messages request's
  `metadata.user_id`, but sets its own on translated requests. The engine correlates by history
  instead: tool-use IDs round-trip unchanged through the Chat and Responses translations, and
  the rest of the history is matched by digest.
- **Tool-call IDs need no hook.** Every MCP `tools/call` from Claude Code carries
  `_meta["claudecode/toolUseId"]`. Claude Code runs a message's MCP calls one at a time, so the
  engine buffers results the harness sends together.
- **Held calls must stream progress.** A held `tools/call` answered as plain JSON times out
  after 60 s, or after 300 s of silence with `MCP_TOOL_TIMEOUT` raised. The environment
  variables page gives `MCP_TOOL_TIMEOUT` a default of about 28 hours, so the 60 s limit on an
  HTTP server's plain JSON reply disagrees with the documentation. SSE comments do not count.
  An SSE response that sends `notifications/progress` held for 330 s without any setting.
- **Sign-in is a command the user runs.** The app shows
  `CLAUDE_CONFIG_DIR=<dir> claude auth login`. It reads the sign-in state from the `account`
  field of Claude Code's `initialize` answer, which makes no API call.

### Engine behaviour

- **Isolation.** `--tools ""`, `--setting-sources ""`, `--strict-mcp-config`,
  `--allowedTools mcp__client__*`, `--permission-mode dontAsk`, `--permission-prompts none` and
  `--settings {"disableAllHooks":true}`. The environment is cleared except for `PATH`, `HOME`,
  `TMPDIR`, `LANG` and `LC_ALL`. It then sets `CLAUDE_CONFIG_DIR` and
  `CLAUDE_CODE_ENTRYPOINT=nonstopvibin`, plus the switches OpenCode uses to turn off
  CLAUDE.md files, auto memory, auto compaction, tool search, the advisor, the non-streaming and
  refusal fallbacks and away summaries. It also sets `CLAUDE_CODE_POLISHED_DEWDROP=drop`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` and `MAX_MCP_OUTPUT_TOKENS=200000`, and
  `CLAUDE_CODE_MAX_OUTPUT_TOKENS` from the request's `max_tokens`. Managed (policy) settings
  still apply, as for any Claude Code.
  - _Added in the build._ Without `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, the real
    Claude Code sent one extra request per new session, a session-title prompt on the
    subscription's model. The spike's drivers set it, so the spike never showed that request.
    `MAX_MCP_OUTPUT_TOKENS` keeps Claude Code from cutting a harness tool's output at its
    MCP default, where the harness's own tools would not cut it.
- **Files.** The profile's Claude Code configuration is `<data>/claude-agent/<profile>`, its
  empty working directory `<data>/claude-agent/work/<profile>`, and each process's system
  prompt and `--mcp-config` file are in `<data>/claude-agent/run/<profile>`, all owner-only.
  The MCP bearer token is new for every process and is only in that file, never in an argument
  another user's `ps` could read. The first request after a daemon start deletes the
  transcripts and process files an earlier run left.
- **Tools.** The request's `tools` become MCP tools named `mcp__client__<name>`. The prefix is
  removed on the way out. Claude Code itself answers a call to a tool that was not offered
  ("No such tool available"). The engine hides that block and its follow-up, so the harness
  sees one response.
- **Sessions.** One `claude` process per conversation. A request is routed by a pure function:
  - **Continue.** Its last message carries the tool results a session is holding. The results
    are handed to Claude Code, and the same turn streams on as this response.
  - **Append.** The rest of its history equals what the session has seen. The new user message
    goes into the live process.
  - **Resume.** The history matches a stopped session, or the model, effort, system prompt or
    tools changed. The engine runs `--resume` with the new settings. A request that carries
    tool results never resumes: no stopped process can still answer its calls.
  - **Fresh.** Anything else, such as an edit, compaction, another model's turns or a lost
    session. The unseen history goes as one `<conversation-checkpoint>` user message.
  - **Side call.** A request without tools, such as a title, runs once with
    `--no-session-persistence`.

  Turns compare by a digest of their role, their text with all whitespace removed, their tool
  calls' ids, and their tool results' ids and text, so the Chat Completions and Responses
  translations, which drop thinking and rejoin text, still match. A turn that fails, is
  interrupted, or runs past what the harness received stops its process without a record, so
  a retry starts fresh.

- **Limits.** At most 3 live processes per profile. The least recently used idle one is stopped
  to make room, and if all are busy the request waits. A process idle for 5 minutes stops. A
  process holding tool calls stops after 30 minutes without the results. Stopped idle sessions
  can be resumed; up to 32 records are kept per profile, and a forgotten one's transcript is
  deleted. One task per profile sleeps until the next of these deadlines and ends when no
  process is live, so an idle daemon keeps no timer. A held call reports progress every 30 s.
- **Orphans.** On Linux each `claude` gets `PR_SET_PDEATHSIG` (SIGKILL) in `pre_exec`, the
  crate's one unsafe call, so it dies with the daemon even after a SIGKILL. On macOS it exits
  once its stdin closes, after the turn in progress. The core's shell watchdog was not reused:
  it guards a process only while `ps` shows exactly the command line it was started with, and
  every `claude` argument list differs. Shutdown stops every process.
- **Errors.** Claude Code retries API errors itself. Its final error becomes the engine's HTTP
  answer: 429 with `retry-after` when Claude Code reported a reset time, 400 "prompt is too
  long" so harnesses compact, 529 when overloaded, and 401 asking the user to sign in again. A
  client that hangs up interrupts the turn.
- **One account.** A profile holds at most one Claude Agent subscription, and never together
  with Claude OAuth seats: connecting beside either, a Claude sign-in or a Claude credential
  import into such a profile all answer 409. Nothing rotates or fails over between Claude
  accounts. A limit is a 429. The account is an API-key account of the `claude-agent` provider:
  its models are the deduplicated `resolvedModel`s of `initialize`, its key is random and only
  the profile's core presents it, and the core's entry points at the daemon's port, computed
  when the config is written.

## Out of scope, kept as follow-ups

- Deprecating or migrating existing Claude OAuth seats. The cloaked route stays available,
  so this is the owner's call.
- Forking a session at the last shared message. A divergence starts fresh with a checkpoint.
- `count_tokens`, images in tool results beyond base64, and Windows.
- Offering this to other users. The Agent SDK overview says third parties may not offer
  claude.ai login "unless previously approved". This is for the owner's own use until Anthropic
  is asked.

## Evidence

- Spike scripts and logs: a fake Anthropic upstream, a fake MCP server and drivers around the
  real `claude` 2.1.326, plus the pinned core 8.1.0 configured as above. Each claim in "What
  changed" was observed there: the system blocks Claude Code sends, the tool-use ID in
  `_meta`, sequential MCP calls, the 60 s and 300 s timeouts against progress
  notifications, the headers and IDs the core forwards, interrupts, and the `result` fields
  for 429 and prompt-too-long errors (`api_error_status`, `terminal_reason`).
- The build's manual check ran the real `claude` 2.1.326 through the real core and the engine
  against the spike's fake upstream: two parallel tool calls round-tripped, a call held for 75 s
  kept Claude Code waiting with progress notifications, and the upstream request carried
  `cc_entrypoint=nonstopvibin` with no cloak additions.
- Not covered here: traffic of a real subscription, a real plan-limit 429 and its reset headers
  (the `rate_limit_event` reset time is read as `rate_limit_info.resetsAt`, unverified), and
  macOS. These need the owner's account and are listed in the pull request.
