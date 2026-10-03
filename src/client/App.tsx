import { ResetCredits } from "./ResetCredits.tsx";
import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  ArrowRight,
  Check,
  Clock3,
  Layers3,
  LoaderCircle,
  Monitor,
  MoreHorizontal,
  Pause,
  Play,
  Plug,
  Plus,
  RefreshCw,
  Settings2,
  ShieldCheck,
  X,
} from "lucide-react";
import type { Account, AppState, ProfileState } from "../shared/types.ts";
import { providerLabel } from "../shared/providers.ts";
import { api, initializeSession } from "./api.ts";
import {
  accountLabel,
  age,
  exactTime,
  floor,
  groupAccountsByProvider,
  isQuotaStale,
  planLabel,
} from "./format.ts";
import {
  AccountSummary,
  AnimatedLogo,
  Logo,
  Modal,
  QuotaMeter,
  Select,
  Status,
  meterLevel,
  useKeepDraft,
} from "./components.tsx";
import { AddSubscription } from "./AddSubscription.tsx";
import { Connect } from "./Connect.tsx";
import { ActivityPage } from "./Activity.tsx";
import { SettingsPage } from "./Settings.tsx";
import { TrayView } from "./Tray.tsx";

type Page = "subscriptions" | "activity" | "connect" | "settings";
const strategyLabel = {
  "round-robin": "round robin",
  "fill-first": "fill first",
};
function Headroom({ groups }: { groups: Map<string, Account[]> }) {
  return (
    <>
      {[...groups].map(([provider, list]) => {
        const known = list
          .map((a) => (a.disabled ? 0 : floor(a)))
          .filter((v): v is number => v !== null);
        const average = known.length
          ? Math.round(known.reduce((s, v) => s + v, 0) / known.length)
          : null;
        const counts = { ready: 0, low: 0, out: 0 };
        for (const a of list) {
          const v = floor(a);
          if (a.disabled || v === 0) counts.out++;
          else if (v !== null && v < 20) counts.low++;
          else counts.ready++;
        }
        return (
          <div className="headroom-provider" key={provider}>
            <strong>
              {providerLabel(provider)}
              <code>
                {list.length} sub{list.length === 1 ? "" : "s"}
              </code>
            </strong>
            <div className={`headroom-value ${meterLevel(average)}`}>
              {average === null ? "—" : average}
              {average !== null && <small>%</small>}
            </div>
            <div className="headroom-segments">
              {list.map((a) => {
                const v = a.disabled ? 0 : floor(a);
                return (
                  <div
                    className={`meter-track ${meterLevel(v)}`}
                    key={a.id}
                    title={`${accountLabel(a)} · ${v === null ? "not checked" : `${Math.round(v)}%`}`}
                  >
                    <div style={{ width: `${v ?? 0}%` }} />
                  </div>
                );
              })}
            </div>
            <code>
              {(
                [
                  ["ready", counts.ready],
                  ["low", counts.low],
                  ["out", counts.out],
                ] as const
              )
                .filter(([, n]) => n > 0)
                .map(([k, n]) => `${n} ${k}`)
                .join(" · ")}
            </code>
          </div>
        );
      })}
    </>
  );
}
export function App() {
  const [state, setState] = useState<AppState>();
  const [selected, setSelected] = useState(
    () =>
      new URLSearchParams(location.search).get("profile") ||
      localStorage.getItem("nv-profile") ||
      "",
  );
  const [page, setPage] = useState<Page>(() => {
    const saved = localStorage.getItem("nv-page");
    return !new URLSearchParams(location.search).has("profile") &&
      (saved === "activity" || saved === "connect" || saved === "settings")
      ? saved
      : "subscriptions";
  });
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState("");
  useKeepDraft(Boolean(busy || error || notice));
  const [createOpen, setCreateOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [reconnecting, setReconnecting] = useState<Account>();
  const [accountId, setAccountId] = useState("");
  const tray = new URLSearchParams(location.search).get("view") === "tray";
  useEffect(() => {
    if (!tray) localStorage.setItem("nv-page", page);
  }, [page, tray]);
  const refresh = useCallback(async () => {
    const next = await api<AppState>("/state");
    setState(next);
  }, []);
  useEffect(() => {
    let mounted = true;
    initializeSession()
      .then(async () => {
        await refresh();
        if (mounted) setReady(true);
      })
      .catch((e) => {
        if (mounted) setError(e.message);
      });
    return () => {
      mounted = false;
    };
  }, [refresh]);
  useEffect(() => {
    if (!ready) return;
    let polling = false;
    const tick = async () => {
      if (document.hidden || polling) return;
      polling = true;
      try {
        await refresh();
      } catch (e) {
        // A restarted server issues a new session token; show the recovery
        // screen rather than a stale workspace.
        if (e instanceof Error && "status" in e && e.status === 401) {
          setReady(false);
          setState(undefined);
        }
        setError(
          e instanceof Error ? e.message : "The local service is unavailable.",
        );
      } finally {
        polling = false;
      }
    };
    let timer: ReturnType<typeof setInterval> | undefined;
    const onVisibilityChange = () => {
      clearInterval(timer);
      if (!document.hidden) {
        void tick();
        timer = setInterval(tick, 4000);
      }
    };
    if (!document.hidden) timer = setInterval(tick, 4000);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [ready, refresh]);
  const profile =
    state?.profiles.find((p) => p.id === selected) ?? state?.profiles[0];
  const accountGroups = groupAccountsByProvider(profile?.accounts ?? []);
  const account = profile?.accounts.find((a) => a.id === accountId);
  async function perform(
    label: string,
    operation: () => Promise<void>,
    success?: string,
  ) {
    setBusy(label);
    setError("");
    try {
      await operation();
      await refresh();
      if (success) setNotice(success);
    } catch (e) {
      setError(e instanceof Error ? e.message : "The operation failed.");
    } finally {
      setBusy("");
    }
  }
  function selectProfile(id: string) {
    setSelected(id);
    localStorage.setItem("nv-profile", id);
    setAccountId("");
    setAddOpen(false);
    setReconnecting(undefined);
    setPage("subscriptions");
    setError("");
  }
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(""), 5000);
    return () => clearTimeout(timer);
  }, [notice]);
  if (!state)
    return (
      <div className="fatal">
        <Logo size={40} word={false} />
        <h1>NonstopVibin</h1>
        {error ? (
          <>
            <p role="alert">{error}</p>
            {/* The server's 401 message already carries this guidance. */}
            {!error.includes("nonstopvibin url") && (
              <p className="muted">
                Open NonstopVibin from the desktop app. On a server, run{" "}
                <code>nonstopvibin url</code> for a new link.
              </p>
            )}
            <button className="button" onClick={() => location.reload()}>
              Try again
            </button>
          </>
        ) : (
          <p>
            <LoaderCircle size={15} className="spin" /> Opening your workspace…
          </p>
        )}
      </div>
    );
  if (tray) return <TrayView state={state} refresh={refresh} />;
  const providers = new Set(profile?.accounts.map((a) => a.provider)).size;
  const subtitle = profile
    ? `${profile.accounts.length} subscription${profile.accounts.length === 1 ? "" : "s"} · ${
        profile.runtime === "running"
          ? strategyLabel[profile.strategy]
          : profile.runtime
      }`
    : "";
  return (
    <div className={`app ${state.desktop ? "desktop" : ""}`}>
      <aside className="sidebar">
        <div className="brand">
          <AnimatedLogo
            state={
              profile?.runtime === "running"
                ? "running"
                : profile?.runtime === "starting"
                  ? "paused"
                  : "stopped"
            }
          />
        </div>
        <div className="sidebar-label">
          <span className="label">Profiles</span>
        </div>
        <nav className="profiles" aria-label="Profiles">
          {state.profiles.map((p) => (
            <button
              key={p.id}
              aria-label={p.name}
              title={p.name}
              onClick={() => selectProfile(p.id)}
              className={`profile-button ${p.id === profile?.id ? "selected" : ""}`}
              aria-current={p.id === profile?.id ? "true" : undefined}
            >
              <span>{p.name}</span>
              <span className="profile-initial" aria-hidden="true">
                {p.name.slice(0, 1)}
              </span>
              <span className="profile-count">{p.accounts.length}</span>
            </button>
          ))}
          <button
            className="profile-button subtle"
            aria-label="New profile"
            title="New profile"
            onClick={() => setCreateOpen(true)}
          >
            <Plus size={13} />
            <span>New profile</span>
          </button>
        </nav>
        <div className="nav-divider" />
        <nav className="main-nav" aria-label="Main navigation">
          {(
            [
              ["subscriptions", "Subscriptions", Layers3],
              ["activity", "Activity", Activity],
              ["connect", "Connect agents", Plug],
            ] as const
          ).map(([id, label, Icon]) => (
            <button
              key={id}
              className={page === id ? "selected" : ""}
              aria-current={page === id ? "page" : undefined}
              title={label}
              onClick={() => {
                setPage(id);
                setAccountId("");
              }}
            >
              <Icon />
              <span>{label}</span>
            </button>
          ))}
        </nav>
        <div className="sidebar-bottom">
          {profile && (
            <div className="proxy-card">
              <Status value={profile.runtime} />
              {profile.runtime === "stopped" || profile.runtime === "error" ? (
                <button
                  className="button small"
                  disabled={Boolean(busy)}
                  onClick={() =>
                    void perform("runtime", () =>
                      api(`/profiles/${profile.id}/start`, "POST"),
                    )
                  }
                >
                  {profile.runtime === "error" ? "Restart" : "Start"}
                </button>
              ) : (
                <code>{state.gateway.replace(/^https?:\/\//, "")}</code>
              )}
            </div>
          )}
          <div className="sidebar-version">
            <button
              className={page === "settings" ? "selected" : ""}
              title="Settings"
              onClick={() => setPage("settings")}
            >
              <Settings2 size={15} />
              <span>Settings</span>
            </button>
            <code>{state.version}</code>
          </div>
        </div>
      </aside>
      <main className="main">
        <div className="window-drag" />
        {error && (
          <div className="global-alert" role="alert">
            <span>{error}</span>
            <button
              className="icon-button"
              aria-label="Dismiss error"
              onClick={() => setError("")}
            >
              <X size={15} />
            </button>
          </div>
        )}
        {notice && (
          <div className="toast" role="status">
            <Check size={15} />
            {notice}
          </div>
        )}
        {page === "settings" ? (
          <SettingsPage
            state={state}
            profile={profile}
            refresh={refresh}
            onError={setError}
          />
        ) : !profile ? (
          <div className="welcome">
            <span className="welcome-icon">
              <Logo size={40} word={false} />
            </span>
            <h1>All your subscriptions, each in its own space.</h1>
            <p>
              Pool the coding subscriptions you already pay for. Keep work
              accounts in a work profile and personal ones in a personal one,
              then point any agent at either.
            </p>
            <button
              className="button primary large"
              onClick={() => setCreateOpen(true)}
            >
              Create your first profile <ArrowRight size={15} />
            </button>
            <div className="welcome-details">
              <span>
                <ShieldCheck size={13} /> separate account pools
              </span>
              <span>
                <Monitor size={13} /> runs on this computer
              </span>
            </div>
          </div>
        ) : (
          <>
            <header className="page-header">
              <div className="page-title">
                <h1>{profile.name}</h1>
                <code>{subtitle}</code>
              </div>
              {page === "subscriptions" && (
                <div className="page-actions">
                  {profile.accounts.length > 0 && (
                    <button
                      className="button"
                      disabled={Boolean(busy) || profile.runtime !== "running"}
                      onClick={() => {
                        void perform(
                          "quotas",
                          () => api(`/profiles/${profile.id}/refresh`, "POST"),
                          "Quota check complete.",
                        );
                      }}
                    >
                      <RefreshCw
                        size={13}
                        className={busy === "quotas" ? "spin" : ""}
                      />
                      {busy === "quotas" ? "Refreshing…" : "Refresh"}
                    </button>
                  )}
                  <button
                    className="button primary"
                    onClick={() => setAddOpen(true)}
                  >
                    <Plus size={14} /> Add subscription
                  </button>
                </div>
              )}
            </header>
            <div className="page-body" key={page}>
              {profile.error && (
                <div className="inline-error" role="alert">
                  {profile.error}
                </div>
              )}
              {page === "subscriptions" &&
                (!profile.accounts.length ? (
                  <div className="empty-state">
                    <div className="empty-icon">
                      <Logo
                        size={28}
                        word={false}
                        state={
                          profile.runtime === "running" ? "running" : "stopped"
                        }
                      />
                    </div>
                    <h2>Nothing pooled yet</h2>
                    <p>
                      Connect the accounts you already pay for. NonstopVibin
                      signs in through each provider’s own OAuth flow and keeps
                      the tokens on this machine.
                    </p>
                    <div className="empty-choices">
                      {(
                        [
                          ["Claude", "5-hour · weekly · opus weekly"],
                          ["Codex", "weekly · credits"],
                          ["Import", "from EasyCLIProxyAPI files"],
                        ] as const
                      ).map(([name, caps]) => (
                        <button key={name} onClick={() => setAddOpen(true)}>
                          <span>
                            <strong>{name}</strong>
                            <code>{caps}</code>
                          </span>
                          <ArrowRight size={14} />
                        </button>
                      ))}
                    </div>
                  </div>
                ) : (
                  <>
                    <div className="headroom">
                      <h2 className="label headroom-heading">
                        Headroom by provider
                      </h2>
                      <Headroom groups={accountGroups} />
                      <div className="headroom-routing">
                        <label className="label" htmlFor="routing">
                          Routing
                        </label>
                        <Select
                          id="routing"
                          value={profile.strategy}
                          disabled={Boolean(busy)}
                          onValueChange={(strategy) => {
                            void perform(
                              "routing",
                              () =>
                                api(`/profiles/${profile.id}`, "PATCH", {
                                  strategy,
                                }),
                              "Routing updated for this profile.",
                            );
                          }}
                          options={[
                            { value: "round-robin", label: "Round robin" },
                            { value: "fill-first", label: "Fill first" },
                          ]}
                        />
                        <p>
                          {profile.strategy === "fill-first"
                            ? "use priority order"
                            : "spread eligible requests"}
                        </p>
                      </div>
                    </div>
                    <div className="accounts">
                      {[...accountGroups].map(([provider, accounts]) => (
                        <section
                          className="provider-group"
                          key={provider}
                          aria-label={`${providerLabel(provider)} subscriptions`}
                        >
                          <div className="accounts-columns">
                            <div className="provider-group-heading">
                              <h2>{providerLabel(provider)}</h2>
                              <span>
                                {accounts.length} subscription
                                {accounts.length === 1 ? "" : "s"}
                              </span>
                            </div>
                            <span className="label">Limits</span>
                            <span />
                          </div>
                          {accounts.map((a) => {
                            // Account-wide limits first; model-only windows fill spare slots.
                            const windows = [...(a.quota?.windows ?? [])]
                              .sort(
                                (x, y) =>
                                  Number(!!x.scoped) - Number(!!y.scoped),
                              )
                              .slice(0, 3);
                            const v = floor(a);
                            const stale = isQuotaStale(a.quota);
                            const tag = a.disabled
                              ? ["Paused", "out"]
                              : a.quota?.status === "error"
                                ? ["Check failed", "bad"]
                                : v === 0
                                  ? ["Out", "out"]
                                  : v !== null && v < 20
                                    ? ["Low", "warn"]
                                    : stale && a.quota
                                      ? ["Stale", ""]
                                      : null;
                            return (
                              <div
                                className={`account-row ${a.disabled ? "disabled" : ""}`}
                                key={a.id}
                              >
                                <button
                                  className="account-button"
                                  onClick={() => setAccountId(a.id)}
                                >
                                  <span className="account-name">
                                    <strong>{accountLabel(a)}</strong>
                                    {tag && (
                                      <span className={`tag ${tag[1]}`}>
                                        {tag[0]}
                                        {tag[0] === "Stale" && (
                                          <code>
                                            {age(a.quota?.checkedAt).replace(
                                              /^checked /,
                                              "",
                                            )}
                                          </code>
                                        )}
                                      </span>
                                    )}
                                  </span>
                                  <span className="account-meta">
                                    <span>
                                      {a.email ??
                                        (a.kind === "api-key"
                                          ? `${a.modelCount ?? 0} model${a.modelCount === 1 ? "" : "s"}`
                                          : "OAuth")}
                                    </span>
                                    {a.quota?.plan && (
                                      <span>{planLabel(a.quota.plan)}</span>
                                    )}
                                  </span>
                                  {a.provider === "codex" &&
                                    a.kind === "oauth" && (
                                      <span
                                        className={`account-resets ${!stale && (a.quota?.bankedResets ?? 0) > 0 ? "available" : ""}`}
                                      >
                                        <RefreshCw
                                          size={13}
                                          aria-hidden="true"
                                        />
                                        {stale ||
                                        a.quota?.bankedResets === undefined
                                          ? "Check banked resets"
                                          : a.quota.bankedResets === 0
                                            ? "No banked resets"
                                            : `${a.quota.bankedResets} banked ${a.quota.bankedResets === 1 ? "reset" : "resets"}`}
                                      </span>
                                    )}
                                </button>
                                <div className="account-limits">
                                  {windows.length ? (
                                    windows.map((w) => (
                                      <QuotaMeter
                                        key={w.label}
                                        window={w}
                                        stale={stale}
                                      />
                                    ))
                                  ) : (
                                    <QuotaMeter
                                      label={
                                        a.quota?.status === "unavailable"
                                          ? "limits"
                                          : "not checked"
                                      }
                                      unavailable={
                                        a.quota?.status === "unavailable"
                                          ? "not reported"
                                          : "refresh to check"
                                      }
                                    />
                                  )}
                                </div>
                                <button
                                  className="icon-button"
                                  aria-label={`Details for ${accountLabel(a)}`}
                                  onClick={() => setAccountId(a.id)}
                                >
                                  <MoreHorizontal size={16} />
                                </button>
                              </div>
                            );
                          })}
                        </section>
                      ))}
                    </div>
                    <div className="accounts-note">
                      <p>
                        {profile.strategy === "fill-first"
                          ? "Fill first sends every request to the highest-priority account with headroom and only moves on when it runs out."
                          : "Round robin skips accounts with no headroom on the limit a request needs."}{" "}
                        Requests never leave {profile.name}.
                      </p>
                      <button
                        className="button"
                        onClick={() => setPage("connect")}
                      >
                        Connect an agent
                      </button>
                    </div>
                  </>
                ))}
              {page === "activity" && (
                <ActivityPage key={profile.id} profile={profile} />
              )}
              {page === "connect" && (
                <Connect key={profile.id} profile={profile} />
              )}
            </div>
          </>
        )}
        <footer className="page-footer">
          <span>
            {profile
              ? `${profile.accounts.length} subscription${profile.accounts.length === 1 ? "" : "s"} · ${providers} provider${providers === 1 ? "" : "s"}`
              : `NonstopVibin ${state.version}`}
          </span>
        </footer>
      </main>
      {createOpen && (
        <CreateProfile
          onClose={() => setCreateOpen(false)}
          onCreated={async (id) => {
            await refresh();
            selectProfile(id);
            setCreateOpen(false);
            setNotice("Profile created. Add a subscription to get started.");
          }}
        />
      )}
      {addOpen && profile && (
        <AddSubscription
          profile={profile}
          profiles={state.profiles}
          reconnectAccount={reconnecting}
          onSelectProfile={selectProfile}
          onClose={() => {
            setAddOpen(false);
            setReconnecting(undefined);
          }}
          onSaved={refresh}
        />
      )}
      {account && profile && (
        <AccountDetails
          key={account.id}
          onReconnect={() => {
            setReconnecting(account);
            setAccountId("");
            setAddOpen(true);
          }}
          account={account}
          profile={profile}
          onClose={() => setAccountId("")}
          refresh={refresh}
        />
      )}
    </div>
  );
}

function CreateProfile({
  onClose,
  onCreated,
}: {
  onClose(): void;
  onCreated(id: string): Promise<void>;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  return (
    <Modal
      open
      title="Create a profile"
      description="A separate group of subscriptions with its own API key."
      onClose={onClose}
    >
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError("");
          try {
            const p = await api<{ id: string }>("/profiles", "POST", {
              name,
              color: "forest",
            });
            await onCreated(p.id);
          } catch (e) {
            setError(
              e instanceof Error ? e.message : "Could not create profile.",
            );
          } finally {
            setBusy(false);
          }
        }}
      >
        {error && (
          <div className="inline-error" role="alert">
            {error}
          </div>
        )}
        <label className="field">
          Profile name
          <input
            autoFocus
            required
            maxLength={60}
            placeholder="Personal, Company A, Side projects…"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        <div className="info-note">
          <ShieldCheck size={18} />
          <p>
            Subscriptions in this profile stay together. Requests never fall
            back to another profile.
          </p>
        </div>
        <div className="modal-footer">
          <button type="button" className="button" onClick={onClose}>
            Cancel
          </button>
          <button className="button primary" disabled={busy || !name.trim()}>
            {busy ? "Creating…" : "Create profile"}
            <ArrowRight size={15} />
          </button>
        </div>
      </form>
    </Modal>
  );
}
function AccountDetails({
  onReconnect,
  account,
  profile,
  onClose,
  refresh,
}: {
  account: Account;
  profile: ProfileState;
  onReconnect(): void;
  onClose(): void;
  refresh(): Promise<void>;
}) {
  const [name, setName] = useState(account.name);
  const [priority, setPriority] = useState(account.priority);
  const [removing, setRemoving] = useState(false);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function save(patch: {
    name?: string;
    priority?: number;
    disabled?: boolean;
  }) {
    setBusy(true);
    setError("");
    try {
      await api(
        `/profiles/${profile.id}/accounts/${encodeURIComponent(account.id)}`,
        "PATCH",
        patch,
      );
      await refresh();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "Could not update subscription.",
      );
    } finally {
      setBusy(false);
    }
  }
  return (
    <Modal
      open
      onClose={onClose}
      title="Subscription details"
      description={`Managed inside ${profile.name}.`}
    >
      <AccountSummary account={account} />
      {account.provider === "claude" && account.kind === "oauth" && (
        <div className="subscription-reconnect">
          <button className="button" onClick={onReconnect} disabled={busy}>
            <RefreshCw size={14} aria-hidden="true" /> Reconnect subscription
          </button>
          <p className="field-note">
            Sign in again to this organization while keeping this connection’s
            history and settings.
          </p>
        </div>
      )}
      {account.organizationUuid && (
        <details>
          <summary>Organization details</summary>
          <dl className="identity-details">
            <dt>Organization</dt>
            <dd>{account.organizationName || "Name not provided"}</dd>
            <dt>Organization ID</dt>
            <dd>{account.organizationUuid}</dd>
            <dt>Account ID</dt>
            <dd>{account.accountUuid || "Not provided"}</dd>
          </dl>
        </details>
      )}
      <div className="detail-status">
        <Status value={account.status} />
        <span>{age(account.quota?.checkedAt)}</span>
      </div>
      {account.quota?.error && (
        <div className="inline-error">
          {account.quota.error}
          {account.quota.windows.length > 0 && (
            <p>Showing the last successful reading.</p>
          )}
        </div>
      )}
      {account.quota?.windows.map((w) => (
        <div className="detail-quota" key={w.label}>
          <QuotaMeter window={w} stale={isQuotaStale(account.quota)} />
          <p className="field-note">
            <Clock3 size={13} />
            {exactTime(w.resetsAt)}
          </p>
        </div>
      ))}
      {!account.quota?.windows.length && (
        <p className="muted">
          {account.quota?.status === "unavailable"
            ? "This provider does not expose a supported quota window. Request and token history is still recorded."
            : "Use Check quotas to fetch the provider’s current limits."}
        </p>
      )}
      {account.provider === "codex" && account.kind === "oauth" && (
        <ResetCredits
          key={`${profile.id}:${account.id}`}
          profileId={profile.id}
          accountId={account.id}
          running={profile.runtime === "running"}
          refresh={refresh}
        />
      )}
      <hr />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          // JSON.stringify drops undefined, so API-key accounts send no priority.
          void save({
            name,
            priority: account.kind === "oauth" ? priority : undefined,
          });
        }}
      >
        <label className="field">
          Label
          <input
            required
            maxLength={60}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        {account.kind === "oauth" && (
          <label className="field">
            Routing priority
            <input
              type="number"
              min={-1000}
              max={1000}
              value={priority}
              onChange={(e) => setPriority(Number(e.target.value))}
            />
            <small>
              Higher numbers are selected first. Equal priorities participate in
              round robin.
            </small>
          </label>
        )}
        {error && (
          <p role="alert" className="inline-error">
            {error}
          </p>
        )}
        <div className="modal-footer">
          <button
            type="button"
            className="button"
            disabled={busy || profile.runtime !== "running"}
            onClick={() => save({ disabled: !account.disabled })}
          >
            {account.disabled ? <Play size={14} /> : <Pause size={14} />}{" "}
            {account.disabled ? "Resume" : "Pause"}
          </button>
          <button
            className="button primary"
            disabled={busy || profile.runtime !== "running"}
          >
            {busy ? "Saving…" : "Save changes"}
          </button>
        </div>
      </form>
      <div className="remove-account">
        {removing ? (
          <>
            <p>
              Remove {accountLabel(account)} from {profile.name}? Its saved
              credentials are deleted here; usage history and any imported
              source files are kept.
            </p>
            <div className="button-row">
              <button
                className="button"
                disabled={busy}
                onClick={() => setRemoving(false)}
              >
                Keep
              </button>
              <button
                className="button danger"
                disabled={busy}
                onClick={async () => {
                  setBusy(true);
                  setError("");
                  try {
                    await api(
                      `/profiles/${profile.id}/accounts/${encodeURIComponent(account.id)}`,
                      "DELETE",
                    );
                    await refresh();
                    onClose();
                  } catch (e) {
                    setError(
                      e instanceof Error
                        ? e.message
                        : "Could not remove account.",
                    );
                  } finally {
                    setBusy(false);
                  }
                }}
              >
                Remove subscription
              </button>
            </div>
          </>
        ) : (
          <button
            className="text-button text-bad"
            onClick={() => setRemoving(true)}
          >
            Remove subscription
          </button>
        )}
      </div>
    </Modal>
  );
}
