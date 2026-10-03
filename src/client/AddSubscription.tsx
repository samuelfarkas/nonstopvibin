import { useEffect, useRef, useState } from "react";
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
import { apiPresets, oauthProviders } from "../shared/providers.ts";
import type {
  Account,
  ImportSource,
  OAuthProvider,
  OAuthReview,
  OAuthStatus,
  OAuthSession,
  ProfileState,
  Json,
  Protocol,
} from "../shared/types.ts";
import { api, openExternal } from "./api.ts";
import {
  AccountSummary,
  CopyButton,
  Modal,
  ProviderIcon,
  Select,
} from "./components.tsx";

import { accountLabel } from "./format.ts";

const hiddenProviders = new Set(["antigravity", "kimi", "xai"]);
const exposedApiPresets = new Set(["opencode-go", "custom"]);

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
  onSaved(): Promise<void>;
}) {
  const [destinationId, setDestinationId] = useState(initialProfile.id);
  const profile =
    profiles.find((p) => p.id === destinationId) ?? initialProfile;
  const [reconnectId, setReconnectId] = useState(reconnectAccount?.id);
  const reconnectTarget = profile.accounts.find((a) => a.id === reconnectId);
  const [claudeOnly, setClaudeOnly] = useState(!!reconnectAccount);
  const claudeAccounts = claudeOnly
    ? profile.accounts.filter(
        (a) => a.provider === "claude" && a.kind === "oauth",
      )
    : [];
  const [review, setReview] = useState<OAuthReview>();
  const [verificationFailed, setVerificationFailed] = useState(false);
  const [selectedTab, setTab] = useState<"oauth" | "api" | "import">("oauth");
  const tab = claudeOnly ? "oauth" : selectedTab;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [session, setSession] = useState<OAuthSession>();
  const [complete, setComplete] =
    useState<Extract<OAuthStatus, { status: "ok" }>>();
  const [presetId, setPresetId] = useState("opencode-go");
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
    api<OAuthSession | null>(`/profiles/${profile.id}/oauth-session`)
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
  }, [profile.id]);
  useEffect(() => {
    if (tab === "import")
      api<ImportSource[]>("/import-sources")
        .then(setSources)
        .catch((e) => setError(e.message));
  }, [tab]);
  async function cancelSignIn() {
    if (!session || complete) return;
    try {
      await api(
        `/profiles/${profile.id}/oauth?state=${encodeURIComponent(session.state)}`,
        "DELETE",
      );
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
  useEffect(() => {
    if (!session || complete || review || verificationFailed) return;
    let cancelled = false;
    let pending = false;
    const timer = setInterval(async () => {
      if (pending || cancelled) return;
      pending = true;
      try {
        const result = await api<OAuthStatus>(
          `/profiles/${profile.id}/oauth?state=${encodeURIComponent(session.state)}`,
        );
        if (!cancelled && result.status === "ok") {
          setComplete(result);
          await onSaved();
        }
        if (!cancelled && result.status === "review") {
          setReview(result.review);
          setError("");
        }
        if (!cancelled && result.status === "error") {
          setError(
            result.error || "Sign-in did not complete. Start a new sign-in.",
          );
          setSession(undefined);
        }
      } catch (e) {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : "Could not check sign-in.");
          setVerificationFailed(true);
        }
      } finally {
        pending = false;
      }
    }, 2000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [session, complete, review, verificationFailed, profile.id, onSaved]);
  async function run(action: () => Promise<void>) {
    setError("");
    setBusy(true);
    try {
      await action();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "Could not connect subscription.",
      );
    } finally {
      setBusy(false);
    }
  }
  async function begin(provider: OAuthProvider) {
    const next = await api<OAuthSession>(
      `/profiles/${profile.id}/oauth`,
      "POST",
      // JSON.stringify drops undefined, so a fresh connection sends no target.
      { provider, reconnectAccountId: reconnectTarget?.id },
    );
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
      open
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
            {session?.provider === "claude" && (
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
          {session?.provider === "claude" && (
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
            {review.action !== "blocked" ? (
              <button
                className="button primary"
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    const result = await api<OAuthStatus>(
                      `/profiles/${profile.id}/oauth-confirm`,
                      "POST",
                      { state: session!.state },
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
            ) : (
              review.existingProfileId && (
                <button
                  className="button primary"
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      await chooseAgain();
                      onSelectProfile(review.existingProfileId!);
                    })
                  }
                >
                  Open existing profile
                </button>
              )
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
            {session.provider === "claude" ? (
              <>
                Choose{" "}
                <strong>
                  {reconnectTarget
                    ? accountLabel(reconnectTarget)
                    : "the organization you want to connect"}
                </strong>{" "}
                on the Claude authorization page. You’ll confirm the selected
                organization here before it joins{" "}
                <strong>{profile.name}</strong>.
              </>
            ) : (
              <>
                Sign in to the account you want to add to{" "}
                <strong>{profile.name}</strong>. This screen will update when
                you finish.
              </>
            )}
          </p>
          {session.provider === "claude" && (
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
            {new Date(
              session.expiresAt ?? session.startedAt + 300_000,
            ).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}
          </p>
          {/* In a browser the server may be remote, so its loopback callback
              cannot be reached; pasting is the expected path there. */}
          <details open={!window.nonstopvibin && !session.userCode}>
            {window.nonstopvibin ? (
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
                  await api(`/profiles/${profile.id}/callback`, "POST", {
                    url: callback,
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
                name="destinationProfile"
                value={profile.id}
                disabled={busy}
                onValueChange={setDestinationId}
                options={profiles.map((p) => ({ value: p.id, label: p.name }))}
              />
            </label>
          )}
          {!claudeOnly && (
            <div className="segmented" aria-label="Connection method">
              {(
                [
                  ["oauth", "Sign in"],
                  ["api", "API key"],
                  ["import", "Import"],
                ] as const
              ).map(([id, label]) => (
                <button
                  key={id}
                  className={tab === id ? "active" : ""}
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
                    !hiddenProviders.has(provider.id) &&
                    (!claudeOnly || provider.id === "claude"),
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
                  await api(`/profiles/${profile.id}/api-account`, "POST", {
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
                  });
                  setKey("");
                  await onSaved();
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
                    setModels(p.models);
                  }}
                  options={apiPresets
                    .filter((provider) => exposedApiPresets.has(provider.id))
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
                    maxLength={60}
                  />
                </label>
                <label className="field">
                  Model prefix
                  <input
                    value={prefix}
                    onChange={(e) => setPrefix(e.target.value)}
                    pattern="[a-z0-9-]*"
                    placeholder="e.g. go"
                    maxLength={30}
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
                    readOnly={presetId === "opencode-go"}
                    onChange={(e) => setBaseUrl(e.target.value)}
                    placeholder="https://api.example.com/v1"
                  />
                </label>
                {presetId !== "opencode-go" && (
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
                          {source.organizationName ||
                            (source.organizationUuid
                              ? `Organization ${source.organizationUuid.slice(0, 8)}`
                              : source.email || source.provider)}
                        </strong>
                        <small>
                          {source.organizationUuid
                            ? `${source.email || source.provider} · ${source.organizationUuid.slice(0, 8)}`
                            : source.provider === "claude"
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
                      for (const sourceId of selectedSources) {
                        await api(
                          `/profiles/${profile.id}/import-source`,
                          "POST",
                          { sourceId },
                        );
                        imported++;
                      }
                      for (const file of importFiles) {
                        if (file.size > 2_000_000)
                          throw new Error(
                            `${file.name} is too large. ${imported} files imported so far.`,
                          );
                        // The server validates the credential; the client only forwards the file.
                        const contents: Json = JSON.parse(await file.text());
                        await api(`/profiles/${profile.id}/import`, "POST", {
                          contents,
                        });
                        imported++;
                      }
                      await onSaved();
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
