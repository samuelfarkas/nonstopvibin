import { useEffect, useState } from "react";
import { ChevronRight, LoaderCircle } from "lucide-react";
import type { AppState, CorePhase, ProfileState } from "../shared/types.ts";
import {
  nameLength,
  retentionDays,
  type RetentionDays,
} from "../shared/vocabulary.ts";
import { api, desktop } from "./api.ts";
import { messageOf } from "./action.ts";
import { Select, useKeepDraft } from "./controls.tsx";
import "./settings.css";

const retentionLabels: Record<RetentionDays, string> = {
  0: "Keep all history",
  30: "30 days",
  90: "90 days",
  365: "1 year",
};
const phaseLabels: Record<CorePhase, string> = {
  checking: "Checking…",
  downloading: "Downloading…",
  testing: "Testing…",
  restarting: "Restarting profiles…",
};

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
    desktop
      ?.getLoginItem()
      .then(setLogin)
      .catch((e) => onError(e.message));
  }, [onError]);
  return (
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
          <CoreSetting state={state} refresh={refresh} onError={onError} />
        </section>
        <section className="settings-section">
          <h3>Desktop</h3>
          <div className="setting-row">
            <div className="setting-copy">
              <h3>Start at login</h3>
              <p>
                {desktop
                  ? "Keep the proxy and quota menu available when you sign in."
                  : "Available in the desktop app, along with the menu-bar quota view."}
              </p>
            </div>
            <input
              type="checkbox"
              role="switch"
              aria-label="Start at login"
              disabled={!desktop}
              checked={login}
              onChange={async (e) => {
                if (!desktop) return;
                try {
                  setLogin(await desktop.setLoginItem(e.target.checked));
                } catch (err) {
                  onError(messageOf(err, "Could not change login setting."));
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
                    await api("PUT /usage-retention", {
                      body: { days: retention },
                    }),
                  );
                } catch (error) {
                  onError(
                    messageOf(error, "Could not change history retention."),
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
                onValueChange={(value) =>
                  setRetention(
                    retentionDays.find((days) => String(days) === value) ??
                      retention,
                  )
                }
                options={retentionDays.map((days) => ({
                  value: String(days),
                  label: retentionLabels[days],
                }))}
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
                To remove integrations too, first disconnect agents and disable
                Start at login.
              </p>
            </div>
            {desktop && (
              <button
                className="button"
                onClick={async () => {
                  try {
                    await desktop?.revealData();
                  } catch (e) {
                    onError(messageOf(e, "Could not open data folder."));
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
  );
}

function CoreSetting({
  state: { core, coreVersion },
  refresh,
  onError,
}: {
  state: AppState;
  refresh(next?: AppState): Promise<void>;
  onError(message: string): void;
}) {
  // The phase reaches state with the next poll; until then, the one this click began.
  const [started, setStarted] = useState<CorePhase>();
  const phase = core.phase ?? started;
  const { available, bundled } = core;
  const installed = coreVersion !== bundled;
  useEffect(() => {
    if (!core.updates) return;
    // Offline, the row keeps what the last check found; opening Settings again retries.
    api("POST /core/check").then(refresh, () => undefined);
  }, [core.updates, refresh]);
  async function change(
    first: CorePhase,
    request: () => Promise<AppState>,
    fallback: string,
  ) {
    setStarted(first);
    try {
      await refresh(await request());
    } catch (error) {
      onError(messageOf(error, fallback));
    } finally {
      setStarted(undefined);
    }
  }
  return (
    <div className="setting-row">
      <div className="setting-copy">
        <h3>CLIProxyAPI</h3>
        {!core.updates ? (
          <p>
            Included with NonstopVibin. Core updates install in installed
            builds.
          </p>
        ) : (
          <>
            {installed && <p>Updated from the bundled {bundled}.</p>}
            {available ? (
              <p>
                Version {available} is available. Updating restarts running
                profiles and interrupts requests in progress.
              </p>
            ) : (
              !installed && (
                <p>
                  Included with NonstopVibin. Reviewed updates install here
                  without an app update.
                </p>
              )
            )}
          </>
        )}
      </div>
      <div className="core-actions">
        {phase ? (
          <button className="button" disabled>
            <LoaderCircle size={15} className="spin" aria-hidden="true" />
            {phaseLabels[phase]}
          </button>
        ) : (
          <>
            {installed && (
              <button
                className="button"
                onClick={() =>
                  change(
                    "restarting",
                    () => api("DELETE /core/install"),
                    "Could not switch to the bundled core.",
                  )
                }
              >
                Use bundled {bundled}
              </button>
            )}
            {available && (
              <button
                className="button primary"
                onClick={() =>
                  change(
                    "checking",
                    () =>
                      api("POST /core/install", {
                        body: { version: available },
                      }),
                    "Could not update the core.",
                  )
                }
              >
                Update to {available}
              </button>
            )}
          </>
        )}
        <code>{coreVersion}</code>
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
                await api("PATCH /profiles/:profileId", {
                  params: { profileId: profile.id },
                  body: { name },
                }),
              );
              setSaved(true);
            } catch (err) {
              onError(messageOf(err, "Rename failed."));
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
            maxLength={nameLength}
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
                await api("PATCH /profiles/:profileId", {
                  params: { profileId: profile.id },
                  body: { sessionAffinity: e.target.checked },
                }),
              );
            } catch (err) {
              onError(messageOf(err, "Update failed."));
            }
          }}
        />
      </div>
    </div>
  );
}
