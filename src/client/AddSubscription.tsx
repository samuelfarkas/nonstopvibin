import { useEffect, useMemo, useRef, useState } from "react";
import "./add-subscription.css";
import {
  ArrowLeft,
  ArrowRight,
  CheckCircle2,
  RefreshCw,
  ExternalLink,
  FileUp,
  KeyRound,
  LoaderCircle,
  Plus,
} from "lucide-react";
import {
  apiPresets,
  oauthProviders,
  type OAuthProvider,
  type Protocol,
  type Provider,
} from "../shared/providers.ts";
import {
  credentialFileBytes,
  nameLength,
  prefixLength,
} from "../shared/vocabulary.ts";
import type {
  AppState,
  ClaudeAgentStatus,
  ImportSource,
  OAuthSession,
  ProfileState,
  Json,
} from "../shared/types.ts";
import { api, desktop, openExternal } from "./api.ts";
import { messageOf, useAction } from "./action.ts";
import { usePolling } from "./feed.tsx";
import { ProviderIcon } from "./components.tsx";
import { CopyButton, Modal, Select } from "./controls.tsx";

export function AddSubscription({
  profile: initialProfile,
  profiles,
  onSelectProfile,
  onClose,
  onSaved,
}: {
  profile: ProfileState;
  profiles: ProfileState[];
  onSelectProfile(id: string): void;
  onClose(): void;
  onSaved(next?: AppState): Promise<void>;
}) {
  const [destinationId, setDestinationId] = useState(initialProfile.id);
  const profile =
    profiles.find((p) => p.id === destinationId) ?? initialProfile;
  const [checkFailed, setCheckFailed] = useState(false);
  const [tab, setTab] = useState<"oauth" | "api" | "import">("oauth");
  const { busy, error, setError, run: runAction } = useAction();
  const run = (action: () => Promise<void>) =>
    runAction(action, "Could not connect subscription.");
  const [session, setSession] = useState<OAuthSession>();
  const [complete, setComplete] = useState(false);
  const [presetId, setPresetId] = useState<Provider>("opencode-go");
  const preset = apiPresets.find((p) => p.id === presetId)!;
  const [name, setName] = useState("OpenCode Go");
  const [baseUrl, setBaseUrl] = useState(preset.baseUrl);
  const [key, setKey] = useState("");
  const [prefix, setPrefix] = useState(preset.prefix);
  const [models, setModels] = useState("");
  const [protocol, setProtocol] = useState<Protocol>("openai");
  const [callback, setCallback] = useState("");
  const fileInput = useRef<HTMLInputElement>(null);
  const [importFiles, setImportFiles] = useState<File[]>([]);
  const fileNames = importFiles.map((file) => file.name).join(", ");
  const [sources, setSources] = useState<ImportSource[]>([]);
  const [selectedSources, setSelectedSources] = useState<string[]>([]);
  // Claude Agent's status, for the profile it was read for.
  const [agentCheck, setAgentCheck] = useState<{
    profileId: string;
    status: ClaudeAgentStatus;
  }>();
  const agent =
    agentCheck?.profileId === profile.id ? agentCheck.status : undefined;
  useEffect(() => {
    let cancelled = false;
    api("GET /profiles/:profileId/oauth-session", {
      params: { profileId: profile.id },
    })
      .then((existing) => {
        if (!cancelled && existing) setSession(existing);
      })
      .catch((e) => {
        if (!cancelled) setError(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [profile.id, setError]);
  useEffect(() => {
    if (tab === "import")
      api("GET /import-sources")
        .then(setSources)
        .catch((e) => setError(e.message));
  }, [tab, setError]);
  async function cancelSignIn() {
    if (!session || complete) return;
    try {
      await api("DELETE /profiles/:profileId/oauth", {
        params: { profileId: profile.id },
        query: { state: session.state },
      });
    } catch (e) {
      if (
        !(e instanceof Error) ||
        !("status" in e) ||
        ![404, 410].includes(Number(e.status))
      )
        throw e;
    }
  }
  async function close() {
    if (busy) return;
    await run(async () => {
      await cancelSignIn();
      if (complete) onSelectProfile(profile.id);
      else onClose();
    });
  }
  const checkSignIn = useMemo(() => {
    if (!session || complete || checkFailed) return;
    return async (signal: AbortSignal) => {
      try {
        const result = await api("GET /profiles/:profileId/oauth", {
          params: { profileId: profile.id },
          query: { state: session.state },
        });
        if (!signal.aborted && result.status === "ok") {
          setComplete(true);
          await onSaved();
        }
        if (!signal.aborted && result.status === "error") {
          setError(
            result.error || "Sign-in did not complete. Start a new sign-in.",
          );
          setSession(undefined);
        }
      } catch (e) {
        if (!signal.aborted) {
          setError(messageOf(e, "Could not check sign-in."));
          setCheckFailed(true);
        }
      }
    };
  }, [session, complete, checkFailed, profile.id, onSaved, setError]);
  // Sign-in happens in a browser that usually covers this window; keep checking
  // while it is hidden so the dialog has the result when the user returns.
  usePolling(checkSignIn, 2000, { whileHidden: true });
  async function begin(provider: OAuthProvider) {
    const next = await api("POST /profiles/:profileId/oauth", {
      params: { profileId: profile.id },
      body: { provider },
    });
    setSession(next);
    setCheckFailed(false);
    await openExternal(next.url);
  }
  async function checkAgent() {
    const status = await api("GET /profiles/:profileId/claude-agent", {
      params: { profileId: profile.id },
    });
    setAgentCheck({ profileId: profile.id, status });
  }
  async function connectAgent() {
    const next = await api("POST /profiles/:profileId/claude-agent", {
      params: { profileId: profile.id },
    });
    await onSaved(next);
    onSelectProfile(profile.id);
  }
  return (
    <Modal
      onClose={() => {
        void close();
      }}
      title={
        complete
          ? "Subscription connected"
          : session
            ? "Finish signing in"
            : "Add a subscription"
      }
      description={`This subscription will belong to ${profile.name}.`}
      wide
    >
      {error && (
        <div className="inline-error" role="alert">
          {error}
        </div>
      )}
      {complete ? (
        <div className="success-state" role="status">
          <CheckCircle2 size={36} aria-hidden="true" />
          <h3>Subscription connected</h3>
          <p>
            Connected in <strong>{profile.name}</strong>.
          </p>
          <div className="button-row">
            <button
              className="button primary"
              disabled={busy}
              onClick={() => onSelectProfile(profile.id)}
            >
              Done
            </button>
          </div>
        </div>
      ) : session ? (
        <div className="oauth-progress">
          <div className="sign-in-illustration">
            <ProviderIcon provider={session.provider} />
            <ArrowRight size={20} />
            <span className="sign-in-target" aria-hidden="true">
              {profile.name.slice(0, 1)}
            </span>
          </div>
          <h3>Continue in your browser</h3>
          <p>
            Sign in to the account you want to add to{" "}
            <strong>{profile.name}</strong>. This screen will update when you
            finish.
          </p>
          <div className="button-row">
            <button
              className="button primary"
              onClick={() => run(() => openExternal(session.url))}
            >
              Open sign-in page <ExternalLink size={14} />
            </button>
            <CopyButton
              text={session.url}
              label="Copy link"
              onError={setError}
            />
          </div>
          {session.userCode && (
            <div className="device-code">
              <p>Enter this code if the provider asks:</p>
              <code>{session.userCode}</code>
              <CopyButton
                text={session.userCode}
                label="Copy code"
                onError={setError}
              />
            </div>
          )}
          {checkFailed && (
            <button
              className="button"
              disabled={busy}
              onClick={() => {
                setError("");
                setCheckFailed(false);
              }}
            >
              <RefreshCw size={14} aria-hidden="true" /> Check again
            </button>
          )}
          <p className="waiting loading" role="status">
            {!checkFailed && (
              <LoaderCircle size={14} className="spin" aria-hidden="true" />
            )}{" "}
            {checkFailed ? "Checking paused" : "Waiting for the provider"}·
            expires at{" "}
            {new Date(session.expiresAt).toLocaleTimeString([], {
              hour: "numeric",
              minute: "2-digit",
            })}
          </p>
          {/* In a browser the server may be remote, so its loopback callback
              cannot be reached; pasting is the expected path there. */}
          <details open={!desktop && !session.userCode}>
            {desktop ? (
              <>
                <summary>Browser did not return to the app?</summary>
                <p>
                  Paste the full localhost callback URL from your browser. Its
                  sign-in state must match this profile.
                </p>
              </>
            ) : (
              <>
                <summary>Page at localhost failed to load?</summary>
                <p>
                  After you sign in, the provider sends your browser to an{" "}
                  <code>http://localhost:…</code> address. When NonstopVibin
                  runs on a remote server, that page fails to load. Copy its
                  full address from the address bar and paste it here.
                </p>
              </>
            )}
            <label className="field">
              Callback URL
              <input
                value={callback}
                onChange={(e) => setCallback(e.target.value)}
                placeholder="http://localhost:…"
              />
            </label>
            <button
              className="button"
              disabled={!callback || busy}
              onClick={() =>
                run(async () => {
                  await api("POST /profiles/:profileId/callback", {
                    params: { profileId: profile.id },
                    body: { url: callback },
                  });
                  setCallback("");
                })
              }
            >
              Complete sign-in
            </button>
          </details>
        </div>
      ) : (
        <>
          <label className="field">
            Destination profile
            <Select
              value={profile.id}
              disabled={busy}
              onValueChange={setDestinationId}
              options={profiles.map((p) => ({ value: p.id, label: p.name }))}
            />
          </label>
          <div
            className="segmented"
            role="group"
            aria-label="Connection method"
          >
            {(
              [
                ["oauth", "Sign in"],
                ["api", "API key"],
                ["import", "Import"],
              ] as const
            ).map(([id, label]) => (
              <button
                type="button"
                key={id}
                className={tab === id ? "active" : ""}
                aria-pressed={tab === id}
                onClick={() => {
                  setTab(id);
                  setError("");
                }}
              >
                {label}
              </button>
            ))}
          </div>
          {tab === "oauth" && agent && (
            <div className="claude-agent">
              <p className="field-note">
                Runs your own signed-in Claude Code for this profile. Claude
                Code makes every request to Claude itself.
              </p>
              {agent.error ? (
                <p className="inline-error" role="alert">
                  {agent.error}
                </p>
              ) : agent.claude ? (
                <div className="device-code">
                  <p>
                    {agent.signedIn
                      ? "Claude Code is signed in. To use another account, run:"
                      : "Sign Claude Code in for this profile in a terminal:"}
                  </p>
                  <code>{agent.signIn}</code>
                  <CopyButton
                    text={agent.signIn}
                    label="Copy command"
                    onError={setError}
                  />
                </div>
              ) : (
                <p className="info-note">
                  Claude Code (claude) was not found. Install it, then choose
                  Claude Agent again.
                </p>
              )}
              <div className="button-row">
                <button
                  className="button"
                  disabled={busy}
                  onClick={() => {
                    setAgentCheck(undefined);
                    setError("");
                  }}
                >
                  <ArrowLeft size={14} aria-hidden="true" /> Back
                </button>
                <button
                  className="button primary"
                  disabled={busy || !agent.claude}
                  onClick={() => run(connectAgent)}
                >
                  {busy ? (
                    <LoaderCircle size={14} className="spin" />
                  ) : (
                    "Connect"
                  )}
                </button>
              </div>
            </div>
          )}
          {tab === "oauth" && !agent && (
            <div className="provider-options">
              <button
                className="provider-option"
                disabled={busy}
                onClick={() => run(checkAgent)}
              >
                <span>
                  <strong>Claude Agent</strong>
                  <code>Your own signed-in Claude Code</code>
                </span>
                {busy ? (
                  <LoaderCircle size={14} className="spin" />
                ) : (
                  <ArrowRight size={14} />
                )}
              </button>
              {oauthProviders
                .filter((provider) => provider.visible)
                .map((provider) => (
                  <button
                    className="provider-option"
                    key={provider.id}
                    disabled={busy}
                    onClick={() => run(() => begin(provider.id))}
                  >
                    <span>
                      <strong>{provider.name}</strong>
                      <code>{provider.detail}</code>
                    </span>
                    {busy ? (
                      <LoaderCircle size={14} className="spin" />
                    ) : (
                      <ArrowRight size={14} />
                    )}
                  </button>
                ))}
              <p className="field-note">
                The provider handles sign-in. NonstopVibin never asks for your
                account password.
              </p>
            </div>
          )}
          {tab === "api" && (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void run(async () => {
                  const next = await api(
                    "POST /profiles/:profileId/api-account",
                    {
                      params: { profileId: profile.id },
                      body: {
                        name,
                        provider: presetId,
                        baseUrl,
                        apiKey: key,
                        protocol,
                        prefix,
                        models: models
                          .split(/[\n,]/)
                          .map((m) => m.trim())
                          .filter(Boolean)
                          .map((id) => ({ id, protocol })),
                      },
                    },
                  );
                  setKey("");
                  await onSaved(next);
                  onSelectProfile(profile.id);
                });
              }}
            >
              <label className="field">
                Service
                <Select
                  value={presetId}
                  onValueChange={(id) => {
                    const p = apiPresets.find((p) => p.id === id)!;
                    setPresetId(p.id);
                    setName(p.name);
                    setBaseUrl(p.baseUrl);
                    setPrefix(p.prefix);
                    setProtocol(p.protocol);
                    setModels("");
                  }}
                  options={apiPresets
                    .filter((provider) => provider.visible)
                    .map((provider) => ({
                      value: provider.id,
                      label: provider.name,
                    }))}
                />
              </label>
              <div className="form-grid">
                <label className="field">
                  Account label
                  <input
                    autoComplete="off"
                    required
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    maxLength={nameLength}
                  />
                </label>
                <label className="field">
                  Model prefix
                  <input
                    value={prefix}
                    onChange={(e) => setPrefix(e.target.value)}
                    pattern="[a-z0-9-]*"
                    placeholder="e.g. go"
                    maxLength={prefixLength}
                  />
                </label>
              </div>
              <label className="field">
                API key
                <input
                  type="password"
                  autoComplete="new-password"
                  required
                  value={key}
                  onChange={(e) => setKey(e.target.value)}
                  placeholder="Paste your key"
                />
              </label>
              <details open={presetId === "custom"}>
                <summary>Endpoint and models</summary>
                <label className="field">
                  Base URL
                  <input
                    type="url"
                    required
                    value={baseUrl}
                    readOnly={preset.fixedEndpoint}
                    onChange={(e) => setBaseUrl(e.target.value)}
                    placeholder="https://api.example.com/v1"
                  />
                </label>
                {!preset.upstream && (
                  <label className="field">
                    API format
                    <Select<Protocol>
                      value={protocol}
                      onValueChange={setProtocol}
                      options={[
                        { value: "openai", label: "OpenAI Chat Completions" },
                        { value: "anthropic", label: "Anthropic Messages" },
                        { value: "responses", label: "OpenAI Responses" },
                      ]}
                    />
                  </label>
                )}
                <label className="field">
                  Model IDs <span className="optional">optional</span>
                  <textarea
                    rows={3}
                    value={models}
                    onChange={(e) => setModels(e.target.value)}
                    placeholder="Leave empty to discover models, or enter one ID per line."
                  />
                </label>
                <p className="field-note">{preset.detail}</p>
              </details>
              <div className="modal-footer">
                <button type="button" className="button" onClick={onClose}>
                  Cancel
                </button>
                <button className="button primary" disabled={busy}>
                  {busy ? (
                    <LoaderCircle size={15} className="spin" />
                  ) : (
                    <Plus size={15} />
                  )}{" "}
                  {busy ? "Connecting…" : "Connect subscription"}
                </button>
              </div>
            </form>
          )}
          {tab === "import" && (
            <div className="import-flow">
              {sources.length > 0 && (
                <div className="detected-imports">
                  <h3>Found on this computer</h3>
                  <p className="field-note">
                    Select the accounts that belong to {profile.name}.
                  </p>
                  {sources.map((source) => (
                    <label className="detected-account" key={source.id}>
                      <input
                        type="checkbox"
                        checked={selectedSources.includes(source.id)}
                        onChange={(e) =>
                          setSelectedSources(
                            e.target.checked
                              ? [...selectedSources, source.id]
                              : selectedSources.filter(
                                  (id) => id !== source.id,
                                ),
                          )
                        }
                      />
                      <span>
                        <strong>{source.email || source.provider}</strong>
                        <small>{source.fileName}</small>
                      </span>
                    </label>
                  ))}
                </div>
              )}
              <div className="import-drop">
                <FileUp size={30} />
                <h3>Bring your existing accounts</h3>
                <p>
                  Choose OAuth JSON files from EasyCLIProxyAPI or another
                  CLIProxyAPI installation.
                </p>
                <input
                  ref={fileInput}
                  type="file"
                  accept=".json,application/json"
                  multiple
                  tabIndex={-1}
                  aria-hidden="true"
                  className="visually-hidden"
                  onChange={(e) => {
                    const files = [...(e.target.files ?? [])];
                    setImportFiles(files);
                  }}
                />
                <button
                  className="button"
                  onClick={() => fileInput.current?.click()}
                >
                  Choose auth files
                </button>
                {fileNames && <p className="selected-files">{fileNames}</p>}
              </div>
              <p className="field-note">
                <KeyRound size={14} /> Only import accounts that belong in{" "}
                {profile.name}.
              </p>
              <p className="muted small-text">
                Files are copied into this profile. Your existing installation
                keeps its original files.
              </p>
              <div className="modal-footer">
                <button className="button" onClick={onClose}>
                  <ArrowLeft size={14} /> Cancel
                </button>
                <button
                  className="button primary"
                  disabled={
                    busy || (!importFiles.length && !selectedSources.length)
                  }
                  onClick={() =>
                    run(async () => {
                      let imported = 0;
                      let next: AppState | undefined;
                      for (const sourceId of selectedSources) {
                        next = await api(
                          "POST /profiles/:profileId/import-source",
                          {
                            params: { profileId: profile.id },
                            body: { sourceId },
                          },
                        );
                        imported++;
                      }
                      for (const file of importFiles) {
                        if (file.size > credentialFileBytes)
                          throw new Error(
                            `${file.name} is too large. ${imported} files imported so far.`,
                          );
                        // The server validates the credential; the client only forwards the file.
                        const contents: Record<string, Json> = JSON.parse(
                          await file.text(),
                        );
                        next = await api("POST /profiles/:profileId/import", {
                          params: { profileId: profile.id },
                          body: { contents },
                        });
                        imported++;
                      }
                      await onSaved(next);
                      onSelectProfile(profile.id);
                    })
                  }
                >
                  {busy
                    ? "Importing…"
                    : `Import${importFiles.length + selectedSources.length ? ` ${importFiles.length + selectedSources.length} account${importFiles.length + selectedSources.length > 1 ? "s" : ""}` : ""}`}
                </button>
              </div>
            </div>
          )}
        </>
      )}
    </Modal>
  );
}
