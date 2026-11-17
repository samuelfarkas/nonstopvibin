import { useEffect, useState } from "react";
import { ChevronRight } from "lucide-react";
import type { AppState, ProfileState } from "../shared/types.ts";
import { api } from "./api.ts";
import { Select, useKeepDraft } from "./components.tsx";
import "./settings.css";

export function SettingsPage({
  state,
  profile: initialProfile,
  refresh,
  onError,
}: {
  state: AppState;
  profile?: ProfileState;
  refresh(next?: AppState): Promise<void>;
  onError(message: string): void;
}) {
  const [login, setLogin] = useState(false);
  const [selectedProfile, setSelectedProfile] = useState(
    initialProfile?.id ?? "",
  );
  const profile =
    state.profiles.find((p) => p.id === selectedProfile) ?? state.profiles[0];
  const [retention, setRetention] = useState(state.usageRetentionDays);
  const [savingRetention, setSavingRetention] = useState(false);
  useKeepDraft(savingRetention || retention !== state.usageRetentionDays);
  useEffect(() => {
    setRetention(state.usageRetentionDays);
  }, [state.usageRetentionDays]);
  useEffect(() => {
    if (window.nonstopvibin)
      window.nonstopvibin
        .getLoginItem()
        .then(setLogin)
        .catch((e) => onError(e.message));
  }, [onError]);
  return (
    <div className="settings-page">
      <header className="page-header">
        <div className="page-title">
          <h1>Settings</h1>
          <code>NonstopVibin {state.version} · local</code>
        </div>
      </header>
      <div className="page-body">
        <div className="settings-content">
          <section
            className="settings-scope"
            aria-labelledby="profile-settings-heading"
          >
            <div className="settings-scope-header">
              <div>
                <h2 id="profile-settings-heading">Profile settings</h2>
                <p>Changes apply only to the selected profile.</p>
              </div>
              {profile && (
                <div className="settings-profile-picker">
                  <label htmlFor="settings-profile">Edit profile</label>
                  <Select
                    id="settings-profile"
                    value={profile.id}
                    onValueChange={setSelectedProfile}
                    options={state.profiles.map((p) => ({
                      value: p.id,
                      label: p.name,
                    }))}
                  />
                </div>
              )}
            </div>
            {profile ? (
              <ProfileSettings
                key={profile.id}
                profile={profile}
                refresh={refresh}
                onError={onError}
              />
            ) : (
              <p className="field-note">
                Create a profile to configure its settings.
              </p>
            )}
          </section>
          <section
            className="settings-scope"
            aria-labelledby="global-settings-heading"
          >
            <div className="settings-scope-header">
              <div>
                <h2 id="global-settings-heading">Global settings</h2>
                <p>Applies to this app and all profiles.</p>
              </div>
            </div>
            <section className="settings-section">
              <h3>Proxy core</h3>
              <div className="setting-row">
                <div className="setting-copy">
                  <h3>CLIProxyAPI</h3>
                  <p>
                    Included with NonstopVibin. Core upgrades arrive with app
                    updates.
                  </p>
                </div>
                <code>{state.coreVersion}</code>
              </div>
            </section>
            <section className="settings-section">
              <h3>Desktop</h3>
              <div className="setting-row">
                <div className="setting-copy">
                  <h3>Start at login</h3>
                  <p>
                    {state.desktop
                      ? "Keep the proxy and quota menu available when you sign in."
                      : "Available in the desktop app, along with the menu-bar quota view."}
                  </p>
                </div>
                <input
                  type="checkbox"
                  role="switch"
                  aria-label="Start at login"
                  disabled={!state.desktop}
                  checked={login}
                  onChange={async (e) => {
                    try {
                      setLogin(
                        await window.nonstopvibin!.setLoginItem(
                          e.target.checked,
                        ),
                      );
                    } catch (err) {
                      onError(
                        err instanceof Error
                          ? err.message
                          : "Could not change login setting.",
                      );
                    }
                  }}
                />
              </div>
              <div className="setting-row">
                <div className="setting-copy">
                  <h3>Closing the window</h3>
                  <p>
                    The desktop app stays in your menu bar. Use Quit to stop all
                    profile proxies.
                  </p>
                </div>
              </div>
            </section>
            <section className="settings-section">
              <h3>Activity history</h3>
              <p className="field-note">
                History stays on this device. Prompts and responses are never
                recorded.
              </p>
              <div className="setting-row">
                <div className="setting-copy">
                  <h3>Keep request history</h3>
                  <p>
                    Applying a limit permanently deletes older activity and its
                    token totals. Cleanup runs daily. Keep all history is the
                    default.
                  </p>
                </div>
                <form
                  className="rename-form"
                  onSubmit={async (event) => {
                    event.preventDefault();
                    setSavingRetention(true);
                    try {
                      await refresh(
                        await api<AppState>("/usage-retention", "PUT", {
                          days: retention,
                        }),
                      );
                    } catch (error) {
                      onError(
                        error instanceof Error
                          ? error.message
                          : "Could not change history retention.",
                      );
                    } finally {
                      setSavingRetention(false);
                    }
                  }}
                >
                  <label className="visually-hidden" htmlFor="usage-retention">
                    Activity history retention
                  </label>
                  <Select
                    id="usage-retention"
                    value={String(retention)}
                    disabled={savingRetention}
                    onValueChange={(value) => setRetention(Number(value))}
                    options={[
                      { value: "0", label: "Keep all history" },
                      { value: "30", label: "30 days" },
                      { value: "90", label: "90 days" },
                      { value: "365", label: "1 year" },
                    ]}
                  />
                  <button
                    className="button"
                    disabled={
                      savingRetention || retention === state.usageRetentionDays
                    }
                  >
                    Apply retention
                  </button>
                </form>
              </div>
            </section>
            <section className="settings-section">
              <div className="setting-row">
                <div className="setting-copy">
                  <h3>Start fresh</h3>
                  <p>
                    Quit NonstopVibin completely, delete its data folder, then
                    reopen the app.
                  </p>
                  <p>
                    To remove integrations too, first disconnect agents and
                    disable Start at login.
                  </p>
                </div>
                {state.desktop && (
                  <button
                    className="button"
                    onClick={async () => {
                      try {
                        await window.nonstopvibin!.revealData();
                      } catch (e) {
                        onError(
                          e instanceof Error
                            ? e.message
                            : "Could not open data folder.",
                        );
                      }
                    }}
                  >
                    Open data folder <ChevronRight size={14} />
                  </button>
                )}
              </div>
            </section>
            {state.errors.length > 0 && (
              <section className="settings-section">
                <h3>Recent service notices</h3>
                <div className="settings-notices">
                  {state.errors.map((e) => (
                    <p key={e} className="inline-error">
                      {e}
                    </p>
                  ))}
                </div>
              </section>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function ProfileSettings({
  profile,
  refresh,
  onError,
}: {
  profile: ProfileState;
  refresh(next?: AppState): Promise<void>;
  onError(message: string): void;
}) {
  const [name, setName] = useState(profile.name);
  const [saved, setSaved] = useState(false);
  const [saving, setSaving] = useState(false);
  useKeepDraft(name !== profile.name || saving);
  useEffect(() => {
    setName(profile.name);
  }, [profile.name]);
  useEffect(() => {
    if (!saved) return;
    const timer = setTimeout(() => setSaved(false), 2500);
    return () => clearTimeout(timer);
  }, [saved]);
  return (
    <div className="settings-section">
      <div className="setting-row">
        <div className="setting-copy">
          <h3>Profile name</h3>
          <p>
            Renaming keeps the same endpoint and API key, so agent setups
            continue to work.
          </p>
        </div>
        <form
          className="rename-form"
          aria-label="Rename profile"
          onSubmit={async (e) => {
            e.preventDefault();
            setSaving(true);
            try {
              await refresh(
                await api<AppState>(`/profiles/${profile.id}`, "PATCH", {
                  name,
                }),
              );
              setSaved(true);
            } catch (err) {
              onError(err instanceof Error ? err.message : "Rename failed.");
            } finally {
              setSaving(false);
            }
          }}
        >
          <label className="visually-hidden" htmlFor="profile-name">
            Profile name
          </label>
          <input
            id="profile-name"
            required
            maxLength={60}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <button
            className="button"
            disabled={saving || name.trim() === profile.name}
          >
            {saving ? "Saving…" : saved ? "Saved" : "Save name"}
          </button>
        </form>
      </div>
      <div className="setting-row">
        <div className="setting-copy">
          <h3>Keep sessions on the same account</h3>
          <p>
            Prefer the same account within a conversation to reuse prompt
            caches. Failover stays in this profile.
          </p>
        </div>
        <input
          type="checkbox"
          role="switch"
          aria-label="Keep sessions on the same account"
          checked={profile.sessionAffinity}
          onChange={async (e) => {
            try {
              await refresh(
                await api<AppState>(`/profiles/${profile.id}`, "PATCH", {
                  sessionAffinity: e.target.checked,
                }),
              );
            } catch (err) {
              onError(err instanceof Error ? err.message : "Update failed.");
            }
          }}
        />
      </div>
    </div>
  );
}
