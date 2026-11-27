import { useEffect, useState } from "react";
import {
  Activity,
  ArrowRight,
  Check,
  Layers3,
  Monitor,
  Plug,
  Plus,
  Settings2,
  ShieldCheck,
  X,
} from "lucide-react";
import type { AppState } from "../shared/types.ts";
import { openedWith } from "../shared/renderer-url.ts";
import { api, desktop } from "./api.ts";
import { messageOf } from "./action.ts";
import { Opening, useAppState } from "./feed.tsx";
import { AnimatedLogo, Logo, logoState, Status } from "./components.tsx";
import { useKeepDraft } from "./controls.tsx";
import {
  strategyCopy,
  SubscriptionActions,
  SubscriptionsPage,
} from "./Subscriptions.tsx";
import { CreateProfile } from "./CreateProfile.tsx";
import { AccountDetails } from "./AccountDetails.tsx";
import { AddSubscription } from "./AddSubscription.tsx";
import { Connect } from "./Connect.tsx";
import { ActivityPage } from "./Activity.tsx";
import { SettingsPage } from "./Settings.tsx";

type Page = "subscriptions" | "activity" | "connect" | "settings";
export function App() {
  const { state, refresh, pollError, setPollError, failure, sessionRefused } =
    useAppState();
  const [selected, setSelected] = useState(
    () =>
      openedWith(location).profile || localStorage.getItem("nv-profile") || "",
  );
  const [page, setPage] = useState<Page>(() => {
    const saved = localStorage.getItem("nv-page");
    return openedWith(location).profile === null &&
      (saved === "activity" || saved === "connect" || saved === "settings")
      ? saved
      : "subscriptions";
  });
  // Action errors stay until dismissed.
  const [error, setError] = useState("");
  const shownError = error || pollError;
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState("");
  useKeepDraft(Boolean(busy || shownError || notice));
  const [createOpen, setCreateOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [accountId, setAccountId] = useState("");
  useEffect(() => {
    localStorage.setItem("nv-page", page);
  }, [page]);
  const profile =
    state?.profiles.find((p) => p.id === selected) ?? state?.profiles[0];
  const account = profile?.accounts.find((a) => a.id === accountId);
  async function perform(
    label: string,
    operation: () => Promise<AppState>,
    success?: string,
  ) {
    setBusy(label);
    setError("");
    try {
      await refresh(await operation());
      if (success) setNotice(success);
    } catch (e) {
      setError(messageOf(e, "The operation failed."));
    } finally {
      setBusy("");
    }
  }
  function selectProfile(id: string) {
    setSelected(id);
    localStorage.setItem("nv-profile", id);
    setAccountId("");
    setAddOpen(false);
    setPage("subscriptions");
    setError("");
  }
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(""), 5000);
    return () => clearTimeout(timer);
  }, [notice]);
  if (!state)
    return <Opening failure={failure} sessionRefused={sessionRefused} />;
  const providers = new Set(profile?.accounts.map((a) => a.provider)).size;
  const heading =
    page === "settings"
      ? { title: "Settings", subtitle: `NonstopVibin ${state.version} · local` }
      : profile && {
          title: profile.name,
          subtitle: `${profile.accounts.length} subscription${profile.accounts.length === 1 ? "" : "s"} · ${
            profile.runtime === "running"
              ? strategyCopy[profile.strategy].label.toLowerCase()
              : profile.runtime
          }`,
        };
  return (
    <div className={`app ${desktop ? "desktop" : ""}`}>
      <aside className="sidebar">
        <div className="brand">
          <AnimatedLogo state={logoState(profile ? [profile] : [])} />
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
                      api("POST /profiles/:profileId/start", {
                        params: { profileId: profile.id },
                      }),
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
              aria-current={page === "settings" ? "page" : undefined}
              aria-label={
                state.core.available
                  ? "Settings, core update available"
                  : undefined
              }
              title="Settings"
              onClick={() => setPage("settings")}
            >
              <Settings2 size={15} />
              <span>Settings</span>
              {state.core.available && (
                <i className="status-dot nav-dot" aria-hidden="true" />
              )}
            </button>
            <code>{state.version}</code>
          </div>
        </div>
      </aside>
      <main className="main">
        {shownError && (
          <div className="global-alert" role="alert">
            <span>{shownError}</span>
            <button
              className="icon-button"
              aria-label="Dismiss error"
              onClick={() => {
                setError("");
                setPollError("");
              }}
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
        {heading ? (
          <>
            <header className="page-header">
              <div className="page-title">
                <h1>{heading.title}</h1>
                <code>{heading.subtitle}</code>
              </div>
              {page === "subscriptions" && profile && (
                <SubscriptionActions
                  profile={profile}
                  busy={busy}
                  perform={perform}
                  onAdd={() => setAddOpen(true)}
                />
              )}
            </header>
            <div className="page-body" key={page}>
              {page === "settings" ? (
                <SettingsPage
                  state={state}
                  profile={profile}
                  refresh={refresh}
                  onError={setError}
                />
              ) : (
                profile && (
                  <>
                    {profile.error && (
                      <div className="inline-error" role="alert">
                        {profile.error}
                      </div>
                    )}
                    {page === "subscriptions" && (
                      <SubscriptionsPage
                        profile={profile}
                        busy={busy}
                        perform={perform}
                        onAdd={() => setAddOpen(true)}
                        onOpenAccount={setAccountId}
                        onConnect={() => setPage("connect")}
                      />
                    )}
                    {page === "activity" && (
                      <ActivityPage key={profile.id} profile={profile} />
                    )}
                    {page === "connect" && (
                      <Connect key={profile.id} profile={profile} />
                    )}
                  </>
                )
              )}
            </div>
          </>
        ) : (
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
          onSelectProfile={selectProfile}
          onClose={() => setAddOpen(false)}
          onSaved={refresh}
        />
      )}
      {account && profile && (
        <AccountDetails
          key={account.id}
          account={account}
          profile={profile}
          onClose={() => setAccountId("")}
          refresh={refresh}
        />
      )}
    </div>
  );
}
