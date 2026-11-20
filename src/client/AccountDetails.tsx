import { useState } from "react";
import { Clock3, Pause, Play, RefreshCw } from "lucide-react";
import type { Account, AppState, ProfileState } from "../shared/types.ts";
import type { AccountPatch } from "../shared/api.ts";
import { providerEntry } from "../shared/providers.ts";
import { nameLength, priorityLimit } from "../shared/vocabulary.ts";
import { api } from "./api.ts";
import { useAction } from "./action.ts";
import { accountLabel, age, exactTime } from "./format.ts";
import { isQuotaStale } from "./quota.ts";
import { AccountSummary, QuotaMeter, Status } from "./components.tsx";
import { Modal } from "./controls.tsx";
import { ResetCredits } from "./ResetCredits.tsx";
import "./account-details.css";

export function AccountDetails({
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
  refresh(next?: AppState): Promise<void>;
}) {
  const [name, setName] = useState(account.name);
  const [priority, setPriority] = useState(account.priority);
  const [removing, setRemoving] = useState(false);
  const { busy, error, run } = useAction();
  const save = (patch: AccountPatch) =>
    run(
      async () =>
        refresh(
          await api("PATCH /profiles/:profileId/accounts/:accountId", {
            params: { profileId: profile.id, accountId: account.id },
            body: patch,
          }),
        ),
      "Could not update subscription.",
    );
  return (
    <Modal
      onClose={onClose}
      title="Subscription details"
      description={`Managed inside ${profile.name}.`}
    >
      <AccountSummary account={account} />
      {providerEntry(account.provider)?.reconnect &&
        account.kind === "oauth" && (
          <div className="subscription-reconnect">
            <button className="button" onClick={onReconnect} disabled={busy}>
              <RefreshCw size={14} aria-hidden="true" /> Reconnect subscription
            </button>
            <p className="field-note">
              {account.status === "Reconnect needed"
                ? "The provider no longer accepts this connection’s sign-in. Reconnect to restore it."
                : "Sign in again to this organization while keeping this connection’s history and settings."}
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
            : "Use Refresh to fetch the provider’s current limits."}
        </p>
      )}
      {providerEntry(account.provider)?.bankedResets &&
        account.kind === "oauth" && (
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
            maxLength={nameLength}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        {account.kind === "oauth" && (
          <label className="field">
            Routing priority
            <input
              type="number"
              min={-priorityLimit}
              max={priorityLimit}
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
                onClick={() =>
                  run(async () => {
                    await refresh(
                      await api(
                        "DELETE /profiles/:profileId/accounts/:accountId",
                        {
                          params: {
                            profileId: profile.id,
                            accountId: account.id,
                          },
                        },
                      ),
                    );
                    onClose();
                  }, "Could not remove account.")
                }
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
