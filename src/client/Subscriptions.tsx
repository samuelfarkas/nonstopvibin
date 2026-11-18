import { ArrowRight, MoreHorizontal, Plus, RefreshCw } from "lucide-react";
import type { Account, AppState, ProfileState } from "../shared/types.ts";
import {
  oauthProviders,
  providerEntry,
  providerLabel,
} from "../shared/providers.ts";
import { api } from "./api.ts";
import {
  accountLabel,
  age,
  groupAccountsByProvider,
  planLabel,
} from "./format.ts";
import {
  accountTag,
  isQuotaStale,
  meterLevel,
  poolHeadroom,
  quotaState,
} from "./quota.ts";
import { Empty, Logo, QuotaMeter } from "./components.tsx";
import { Select } from "./controls.tsx";
import { strategies } from "../shared/vocabulary.ts";
import "./subscriptions.css";

/** Each routing strategy's name and what it does, in the picker and around it. */
export const strategyCopy = {
  "round-robin": {
    label: "Round robin",
    summary: "spread eligible requests",
    note: "Round robin skips accounts with no headroom on the limit a request needs.",
  },
  "fill-first": {
    label: "Fill first",
    summary: "use priority order",
    note: "Fill first sends every request to the highest-priority account with headroom and only moves on when it runs out.",
  },
} satisfies Record<
  ProfileState["strategy"],
  { label: string; summary: string; note: string }
>;

/** Runs a profile change that answers the new state, with a busy label and a success notice. */
export type Perform = (
  label: string,
  operation: () => Promise<AppState>,
  success?: string,
) => Promise<void>;

function Headroom({ groups }: { groups: Map<string, Account[]> }) {
  return (
    <>
      {[...groups].map(([provider, list]) => {
        const { average, values, counts } = poolHeadroom(list);
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
              {list.map((a, index) => {
                const v = values[index];
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

export function SubscriptionActions({
  profile,
  busy,
  perform,
  onAdd,
}: {
  profile: ProfileState;
  busy: string;
  perform: Perform;
  onAdd(): void;
}) {
  return (
    <div className="page-actions">
      {profile.accounts.length > 0 && (
        <button
          className="button"
          // With a subscription the app keeps the profile running: Refresh starts it if needed.
          disabled={Boolean(busy)}
          onClick={() => {
            void perform(
              "quotas",
              () =>
                api("POST /profiles/:profileId/refresh", {
                  params: { profileId: profile.id },
                }),
              "Quota check complete.",
            );
          }}
        >
          <RefreshCw size={13} className={busy === "quotas" ? "spin" : ""} />
          {busy === "quotas" ? "Refreshing…" : "Refresh"}
        </button>
      )}
      <button className="button primary" onClick={() => onAdd()}>
        <Plus size={14} /> Add subscription
      </button>
    </div>
  );
}

export function SubscriptionsPage({
  profile,
  busy,
  perform,
  onAdd,
  onOpenAccount,
  onConnect,
}: {
  profile: ProfileState;
  busy: string;
  perform: Perform;
  onAdd(): void;
  onOpenAccount(id: string): void;
  onConnect(): void;
}) {
  const accountGroups = groupAccountsByProvider(profile.accounts);
  return !profile.accounts.length ? (
    <Empty
      icon={
        <Logo
          size={28}
          word={false}
          state={profile.runtime === "running" ? "running" : "stopped"}
        />
      }
      title="Nothing pooled yet"
    >
      <p>
        Connect the accounts you already pay for. NonstopVibin signs in through
        each provider’s own OAuth flow and keeps the tokens on this machine.
      </p>
      <div className="empty-choices">
        {[
          ...oauthProviders
            .filter((provider) => provider.visible)
            .map((provider) => [provider.label, provider.limits ?? ""]),
          ["Import", "from EasyCLIProxyAPI files"],
        ].map(([name, caps]) => (
          <button key={name} onClick={() => onAdd()}>
            <span>
              <strong>{name}</strong>
              <code>{caps}</code>
            </span>
            <ArrowRight size={14} />
          </button>
        ))}
      </div>
    </Empty>
  ) : (
    <>
      <div className="headroom">
        <h2 className="label headroom-heading">Headroom by provider</h2>
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
                  api("PATCH /profiles/:profileId", {
                    params: { profileId: profile.id },
                    body: { strategy },
                  }),
                "Routing updated for this profile.",
              );
            }}
            options={strategies.map((value) => ({
              value,
              label: strategyCopy[value].label,
            }))}
          />
          <p>{strategyCopy[profile.strategy].summary}</p>
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
              const windows = [...(a.quota?.windows ?? [])]
                .sort((x, y) => Number(!!x.scoped) - Number(!!y.scoped))
                .slice(0, 3);
              const stale = isQuotaStale(a.quota);
              const tag = accountTag(a);
              return (
                <div
                  className={`account-row ${a.disabled ? "disabled" : ""}`}
                  key={a.id}
                >
                  <button
                    className="account-button"
                    onClick={() => onOpenAccount(a.id)}
                  >
                    <span className="account-name">
                      <strong>{accountLabel(a)}</strong>
                      {tag && (
                        <span className={`tag ${tag.tone}`}>
                          {tag.text}
                          {tag.text === quotaState.stale && (
                            <code>
                              {age(a.quota?.checkedAt).replace(/^checked /, "")}
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
                      {a.quota?.plan && <span>{planLabel(a.quota.plan)}</span>}
                    </span>
                    {providerEntry(a.provider)?.bankedResets &&
                      a.kind === "oauth" && (
                        <span
                          className={`account-resets ${!stale && (a.quota?.bankedResets ?? 0) > 0 ? "available" : ""}`}
                        >
                          <RefreshCw size={13} aria-hidden="true" />
                          {stale || a.quota?.bankedResets === undefined
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
                        <QuotaMeter key={w.label} window={w} stale={stale} />
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
                    onClick={() => onOpenAccount(a.id)}
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
          {strategyCopy[profile.strategy].note} Requests never leave{" "}
          {profile.name}.
        </p>
        <button className="button" onClick={() => onConnect()}>
          Connect an agent
        </button>
      </div>
    </>
  );
}
