import { useEffect, useMemo, useRef, useState } from "react";
import "./add-subscription.css";
import {
  ArrowLeft,
  ArrowRight,
  CheckCircle2,
  Building2,
  RefreshCw,
  ShieldCheck,
  ExternalLink,
  FileUp,
  KeyRound,
  LoaderCircle,
  Plus,
} from "lucide-react";
import {
  apiPresets,
  oauthProviders,
  providerEntry,
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
  Account,
  AppState,
  ImportSource,
  OAuthReview,
  OAuthStatus,
  OAuthSession,
  ProfileState,
  Json,
} from "../shared/types.ts";
import { api, desktop, openExternal } from "./api.ts";
import { messageOf, useAction } from "./action.ts";
import { usePolling } from "./feed.tsx";
import { AccountSummary, ProviderIcon } from "./components.tsx";
import { CopyButton, Modal, Select } from "./controls.tsx";

import { accountLabel, organizationLabel } from "./format.ts";

/** Whether accounts of this provider are seats in separate organizations. */
const organizations = (provider: string) =>
  providerEntry(provider)?.organizations === true;

export function AddSubscription({
  profile: initialProfile,
  profiles,
  reconnectAccount,
  onSelectProfile,
  onClose,
  onSaved,
}: {
  profile: ProfileState;
  profiles: ProfileState[];
  reconnectAccount?: Account;
  onSelectProfile(id: string): void;
  onClose(): void;
  onSaved(next?: AppState): Promise<void>;
}) {
  const [destinationId, setDestinationId] = useState(initialProfile.id);
  const profile =
    profiles.find((p) => p.id === destinationId) ?? initialProfile;
  const [reconnectId, setReconnectId] = useState(reconnectAccount?.id);
  const reconnectTarget = profile.accounts.find((a) => a.id === reconnectId);
  const [claudeOnly, setClaudeOnly] = useState(!!reconnectAccount);
  const claudeAccounts = claudeOnly
    ? profile.accounts.filter(
        (a) => organizations(a.provider) && a.kind === "oauth",
      )
    : [];
  const [review, setReview] = useState<OAuthReview>();
  const [verificationFailed, setVerificationFailed] = useState(false);
  const [selectedTab, setTab] = useState<"oauth" | "api" | "import">("oauth");
  const tab = claudeOnly ? "oauth" : selectedTab;
  const { busy, error, setError, run: runAction } = useAction();
  const run = (action: () => Promise<void>) =>
    runAction(action, "Could not connect subscription.");
  const [session, setSession] = useState<OAuthSession>();
  const [complete, setComplete] =
    useState<Extract<OAuthStatus, { status: "ok" }>>();
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
  useEffect(() => {
    let cancelled = false;
    api("GET /profiles/:profileId/oauth-session", {
      params: { profileId: profile.id },
    })
      .then((existing) => {
        if (!cancelled && existing) {
          setSession(existing);
          setReconnectId(existing.reconnectAccountId);
        }
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
    if (!session || complete || review || verificationFailed) return;
    return async (signal: AbortSignal) => {
      try {
        const result = await api("GET /profiles/:profileId/oauth", {
          params: { profileId: profile.id },
          query: { state: session.state },
        });
        if (!signal.aborted && result.status === "ok") {
          setComplete(result);
          await onSaved();
        }
        if (!signal.aborted && result.status === "review") {
          if (result.review.action === "reconnect" && reconnectId) {
            // The verified seat already matches the chosen connection; a confirm
            // click adds nothing and users miss it. Fall back to manual review on failure.
            let confirmed: OAuthStatus;
            try {
              confirmed = await api("POST /profiles/:profileId/oauth-confirm", {
                params: { profileId: profile.id },
                body: { state: session.state },
              });
              if (confirmed.status !== "ok")
                throw new Error("The connection was not completed. Try again.");
            } catch (e) {
              if (signal.aborted) return;
              setReview(result.review);
              setError(messageOf(e, "Could not reconnect the subscription."));
              return;
            }
            // The server has already replaced the credential; show the result even
            // if a profile refresh restarted this poll while confirming.
            setComplete(confirmed);
            await onSaved();
            return;
          }
          setReview(result.review);
          setError("");
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
          setVerificationFailed(true);
        }
      }
    };
  }, [
    session,
    complete,
    review,
    verificationFailed,
    profile.id,
    reconnectId,
    onSaved,
    setError,
  ]);
  // Sign-in happens in a browser that usually covers this window; keep checking
  // while it is hidden so the dialog has the result when the user returns.
  usePolling(checkSignIn, 2000, { whileHidden: true });
  async function begin(provider: OAuthProvider) {
    const next = await api("POST /profiles/:profileId/oauth", {
      params: { profileId: profile.id },
      // JSON.stringify drops undefined, so a fresh connection sends no target.
      body: { provider, reconnectAccountId: reconnectTarget?.id },
    });
    setSession(next);
    setReview(undefined);
    setVerificationFailed(false);
    await openExternal(next.url);
  }
  async function chooseAgain() {
    await cancelSignIn();
    setSession(undefined);
    setReview(undefined);
    setVerificationFailed(false);
    setCallback("");
    setClaudeOnly(true);
  }
  return (
    <Modal
      onClose={() => {
        void close();
      }}
      title={
        complete
          ? complete.reconnected
            ? "Subscription reconnected"
            : "Subscription connected"
          : review
            ? "Confirm your organization"
            : session
              ? "Finish signing in"
              : reconnectTarget
                ? "Reconnect your subscription"
                : claudeOnly
                  ? "Connect another organization"
                  : "Add a subscription"
      }
      description={
        review
          ? "Check the organization before connecting it to your profile."
          : `This subscription will belong to ${profile.name}.`
      }
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
          <h3>
            {complete.account
              ? accountLabel(complete.account)
              : "Subscription connected"}
          </h3>
          <p>
            {complete.account?.disabled
              ? "Your subscription is reconnected and still paused"
              : "Connected"}{" "}
            in <strong>{profile.name}</strong>.
          </p>
          {complete.account && <AccountSummary account={complete.account} />}
          <div className="button-row">
            <button
              className="button primary"
              disabled={busy}
              onClick={() => onSelectProfile(profile.id)}
            >
              Done
            </button>
            {session && organizations(session.provider) && (
              <button
                className="button"
                disabled={busy}
                onClick={() => {
                  setComplete(undefined);
                  setSession(undefined);
                  setReview(undefined);
                  setReconnectId(undefined);
                  setClaudeOnly(true);
                }}
              >
                <Plus size={15} aria-hidden="true" /> Connect another
                organization
              </button>
            )}
          </div>
          {session && organizations(session.provider) && (
            <p className="field-note">
              Use the same email and choose a different organization when you
              sign in.
            </p>
          )}
        </div>
      ) : review ? (
        <div className="organization-review">
          <div className="verified-organization">
            <ShieldCheck size={16} aria-hidden="true" /> Organization verified
            by Claude
          </div>
          <AccountSummary account={review.account} />
          <div className="organization-destination">
            <Building2 size={18} aria-hidden="true" />
            <span>
              {review.action === "reconnect" ? "Reconnect in" : "Connect to"}
              <strong>{profile.name}</strong>
            </span>
          </div>
          {review.action === "blocked" ? (
            <p className="inline-error" role="alert">
              {review.message}
            </p>
          ) : (
            <p className="field-note">
              {review.action === "reconnect"
                ? "This seat is already in this profile. Its history, label, priority, and paused state will stay in place. A running profile briefly restarts to apply the new sign-in."
                : "This organization gets its own connection and quota readings, even when another seat uses the same email."}
            </p>
          )}
          <details>
            <summary>Organization details</summary>
            <dl className="identity-details">
              <dt>Organization ID</dt>
              <dd>{review.account.organizationUuid}</dd>
              <dt>Account ID</dt>
              <dd>{review.account.accountUuid}</dd>
            </dl>
          </details>
          <div className="modal-footer">
            <button
              className="button"
              disabled={busy}
              onClick={() => run(chooseAgain)}
            >
              Choose another organization
            </button>
            {review.action !== "blocked" && (
              <button
                className="button primary"
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    const result = await api(
                      "POST /profiles/:profileId/oauth-confirm",
                      {
                        params: { profileId: profile.id },
                        body: { state: session!.state },
                      },
                    );
                    if (result.status !== "ok")
                      throw new Error(
                        "The connection was not completed. Try again.",
                      );
                    setComplete(result);
                    await onSaved();
                  })
                }
              >
                {busy ? (
                  <LoaderCircle size={15} className="spin" aria-hidden="true" />
                ) : (
                  <CheckCircle2 size={15} aria-hidden="true" />
                )}
                {busy
                  ? "Connecting…"
                  : review.action === "reconnect"
                    ? "Reconnect subscription"
                    : "Connect organization"}
              </button>
            )}
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
            {organizations(session.provider) ? (
              <>
                Choose{" "}
                <strong>
                  {reconnectTarget
                    ? accountLabel(reconnectTarget)
                    : "the organization you want to connect"}
                </strong>{" "}
                on the Claude authorization page.{" "}
                {reconnectTarget ? (
                  <>
                    After you sign in, the reconnect in{" "}
                    <strong>{profile.name}</strong> finishes here on its own.
                  </>
                ) : (
                  <>
                    You’ll confirm the selected organization here before it
                    joins <strong>{profile.name}</strong>.
                  </>
                )}
              </>
            ) : (
              <>
                Sign in to the account you want to add to{" "}
                <strong>{profile.name}</strong>. This screen will update when
                you finish.
              </>
            )}
          </p>
          {organizations(session.provider) && (
            <p className="organization-hint">
              One email can have several seats. Each organization connects
              separately.
            </p>
          )}
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
          {verificationFailed && (
            <button
              className="button"
              disabled={busy}
              onClick={() => {
                setError("");
                setVerificationFailed(false);
              }}
            >
              <RefreshCw size={14} aria-hidden="true" /> Retry verification
            </button>
          )}
          <p className="waiting loading" role="status">
            {!verificationFailed && (
              <LoaderCircle size={14} className="spin" aria-hidden="true" />
            )}{" "}
            {verificationFailed
              ? "Verification paused"
              : "Waiting for the provider"}
            · expires at{" "}
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
          {reconnectTarget ? (
            <div className="reconnect-intro">
              <AccountSummary account={reconnectTarget} />
              <p className="field-note">
                Sign in with the same account and organization. Your other seats
                will stay connected.
              </p>
            </div>
          ) : (
            <label className="field">
              Destination profile
              <Select
                value={profile.id}
                disabled={busy}
                onValueChange={setDestinationId}
                options={profiles.map((p) => ({ value: p.id, label: p.name }))}
              />
            </label>
          )}
          {!claudeOnly && (
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
          )}
          {tab === "oauth" && (
            <div className="provider-options">
              {oauthProviders
                .filter(
                  (provider) =>
                    provider.visible && (!claudeOnly || provider.organizations),
                )
                .map((provider) => (
                  <button
                    className="provider-option"
                    key={provider.id}
                    disabled={busy}
                    onClick={() => run(() => begin(provider.id))}
                  >
                    <span>
                      <strong>
                        {reconnectTarget
                          ? "Sign in to reconnect"
                          : provider.name}
                      </strong>
                      <code>{provider.detail}</code>
                    </span>
                    {busy ? (
                      <LoaderCircle size={14} className="spin" />
                    ) : (
                      <ArrowRight size={14} />
                    )}
                  </button>
                ))}
              {claudeAccounts.length > 0 && (
                <div className="connected-organizations">
                  <h3>Already connected in {profile.name}</h3>
                  {claudeAccounts.map((a) => (
                    <div key={a.id}>
                      <CheckCircle2 size={14} aria-hidden="true" />
                      <span>
                        {accountLabel(a)}
                        <small>{a.email}</small>
                      </span>
                    </div>
                  ))}
                </div>
              )}
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
                {!preset.fixedEndpoint && (
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
                        <strong>
                          {organizationLabel(source) ||
                            source.email ||
                            source.provider}
                        </strong>
                        <small>
                          {source.organizationUuid
                            ? `${source.email || source.provider} · ${source.organizationUuid.slice(0, 8)}`
                            : organizations(source.provider)
                              ? "Organization unverified"
                              : source.fileName}
                        </small>
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
