import "./activity.css";
import { activityBuckets } from "./activity-buckets.ts";
import { activityModels } from "./activity-models.ts";
import { ModelTokenChart } from "./ModelTokenChart.tsx";

import { useEffect, useState } from "react";
import { Download, Search } from "lucide-react";
import type {
  ProfileState,
  UsageRecord,
  UsageSummary,
  UsagePricing,
} from "../shared/types.ts";
import { providerLabel } from "../shared/providers.ts";
import { api } from "./api.ts";
import { Empty } from "./components.tsx";
import { accountLabel, count, csvCell, dateLabel } from "./format.ts";

const ranges = [
  { days: 1, label: "24h" },
  { days: 7, label: "7d" },
  { days: 30, label: "30d" },
  { days: 3650, label: "all" },
] as const;

const percentage = (part: number, whole: number, digits = 0) =>
  whole ? `${((part / whole) * 100).toFixed(digits)}%` : "0%";

const percentile = (values: number[], value: number) => {
  if (!values.length) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.max(0, Math.ceil(sorted.length * value) - 1)];
};

export function ActivityPage({ profile }: { profile: ProfileState }) {
  const [days, setDays] = useState(7);
  const [records, setRecords] = useState<UsageRecord[]>([]);
  const [summary, setSummary] = useState<UsageSummary>();
  const [pricing, setPricing] = useState<UsagePricing>();
  const [currency, setCurrency] = useState<"USD" | "EUR">("USD");
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const [failures, setFailures] = useState(false);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let stopped = false;
    let busy = false;
    let initial = true;
    setLoading(true);
    setRecords([]);
    setSummary(undefined);
    setPricing(undefined);
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        const data = await api<{
          records: UsageRecord[];
          summary: UsageSummary;
          pricing?: UsagePricing;
        }>(`/profiles/${profile.id}/usage?days=${days}`);
        if (!stopped) {
          setRecords(data.records);
          setSummary(data.summary);
          setPricing(data.pricing);
          setError("");
        }
      } catch (e) {
        if (!stopped)
          setError(e instanceof Error ? e.message : "Could not load activity.");
      } finally {
        busy = false;
        if (initial && !stopped) setLoading(false);
        initial = false;
      }
    };
    void load();
    let timer: ReturnType<typeof setInterval> | undefined;
    const onVisibilityChange = () => {
      clearInterval(timer);
      if (!document.hidden) {
        void load();
        timer = setInterval(load, 5000);
      }
    };
    if (!document.hidden) timer = setInterval(load, 5000);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      stopped = true;
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [profile.id, days]);

  // Reuse within a render, while picking up timezone changes on the next refresh.
  const rowTime = new Intl.DateTimeFormat([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
  const rowTimestamp = new Intl.DateTimeFormat([], {
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    second: "numeric",
  });
  const accounts = new Map(
    profile.accounts.map((account) => [
      account.authIndex || account.id,
      account,
    ]),
  );
  const subscription = (record: UsageRecord) => {
    const account = accounts.get(record.account);
    return account ? accountLabel(account) : "";
  };
  const recordAccount = (record: UsageRecord) =>
    subscription(record) || providerLabel(record.provider);
  const filtered = records.filter(
    (record) =>
      (!failures || record.failed) &&
      `${record.model} ${providerLabel(record.provider)} ${subscription(record)} ${accounts.get(record.account)?.email ?? ""}`
        .toLowerCase()
        .includes(query.toLowerCase()),
  );

  const money = new Intl.NumberFormat([], {
    style: "currency",
    currency,
    maximumFractionDigits: 4,
  });
  const amount = (usd: number | null | undefined) => {
    if (usd == null || (currency === "EUR" && !pricing?.eur)) return "—";
    return money.format(
      usd * (currency === "EUR" ? (pricing?.eur?.rate ?? 0) : 1),
    );
  };
  const costOf = (record: UsageRecord) => pricing?.usd[record.id] ?? null;
  const totals = pricing?.totals;
  const groupAmount = (usd: number, pricedCount: number, requests: number) =>
    `${amount(pricedCount ? usd : null)}${pricedCount > 0 && pricedCount < requests ? " + ?" : ""}`;

  const totalRequests = summary?.requests ?? 0;
  const failedRequests = summary?.failed ?? 0;
  const totalTokens = summary?.totalTokens ?? 0;
  // "all" has no fixed window and the sample is capped at 500, so a daily
  // rate is only honest when the sample covers the whole history.
  const elapsedDays =
    days !== 3650
      ? days
      : records.length && records.length < 500
        ? Math.max(
            1,
            (Date.now() -
              Math.min(
                ...records.map((record) =>
                  new Date(record.timestamp).getTime(),
                ),
              )) /
              86_400_000,
          )
        : null;
  const failedCodes = new Map<number, number>();
  for (const record of records) {
    if (record.failed)
      failedCodes.set(
        record.statusCode,
        (failedCodes.get(record.statusCode) ?? 0) + 1,
      );
  }
  const topStatus = [...failedCodes].sort((a, b) => b[1] - a[1])[0];
  const tokens = totals?.tokens;
  const cacheDenominator =
    (tokens?.input ?? 0) + (tokens?.cached ?? 0) + (tokens?.cacheWrite ?? 0);
  const completeTokens = totals?.normalizedRequests === totalRequests;
  const cacheRate = completeTokens
    ? cacheDenominator
      ? ((tokens?.cached ?? 0) / cacheDenominator) * 100
      : 0
    : null;
  const p95Latency = percentile(
    records.map((record) => record.latencyMs),
    0.95,
  );
  const tokenParts = [
    { label: "cached", value: tokens?.cached ?? 0, className: "cached" },
    {
      label: "cache write",
      value: tokens?.cacheWrite ?? 0,
      className: "cache-write",
    },
    { label: "input", value: tokens?.input ?? 0, className: "input" },
    { label: "output", value: tokens?.output ?? 0, className: "output" },
    {
      label: "reasoning",
      value: tokens?.reasoning ?? 0,
      className: "reasoning",
    },
  ];
  const unclassified = Math.max(
    0,
    totalTokens - tokenParts.reduce((sum, part) => sum + part.value, 0),
  );
  if (unclassified)
    tokenParts.push({
      label: "unclassified",
      value: unclassified,
      className: "unclassified",
    });
  const tokenMixTotal = tokenParts.reduce((sum, part) => sum + part.value, 0);
  const buckets = activityBuckets(records, days);
  const maxBucket = Math.max(1, ...buckets.map((bucket) => bucket.total));

  const accountGroups = new Map<
    string,
    {
      label: string;
      requests: number;
      latency: number;
      failed: number;
      usd: number;
      priced: number;
    }
  >();
  for (const record of records) {
    const key = record.account || record.provider;
    const group = accountGroups.get(key) ?? {
      label: recordAccount(record),
      usd: 0,
      priced: 0,
      requests: 0,
      latency: 0,
      failed: 0,
    };
    group.requests += 1;
    group.latency += record.latencyMs;
    group.usd += costOf(record) ?? 0;
    group.priced += Number(costOf(record) !== null);
    if (record.failed) group.failed += 1;
    accountGroups.set(key, group);
  }
  const byAccount = [...accountGroups.values()].sort(
    (a, b) => b.requests - a.requests,
  );

  const byModel = activityModels(records, pricing?.usd);

  function exportCSV() {
    const keys: Array<keyof UsageRecord> = [
      "timestamp",
      "provider",
      "account",
      "model",
      "inputTokens",
      "outputTokens",
      "cachedTokens",
      "cacheWriteTokens",
      "reasoningTokens",
      "totalTokens",
      "latencyMs",
      "statusCode",
    ];
    const csv = [
      [
        ...keys,
        "estimatedUsd",
        "estimatedEur",
        "pricingFetchedAt",
        "eurRateDate",
      ].join(","),
      ...filtered.map((record) =>
        [
          ...keys.map((key) => csvCell(String(record[key]))),
          costOf(record) ?? "",
          costOf(record) !== null && pricing?.eur
            ? (costOf(record) ?? 0) * pricing.eur.rate
            : "",
          csvCell(pricing?.fetchedAt ?? ""),
          csvCell(pricing?.eur?.date ?? ""),
        ].join(","),
      ),
    ].join("\n");
    const url = URL.createObjectURL(new Blob([csv], { type: "text/csv" }));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `nonstopvibin-${profile.slug}-activity.csv`;
    anchor.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  return (
    <div className="activity-page">
      <div className="activity-range">
        <h2 className="label" id="usage-title">
          Usage overview
        </h2>
        <div className="segmented" aria-label="Time range">
          {ranges.map((range) => (
            <button
              type="button"
              className={days === range.days ? "active" : ""}
              aria-pressed={days === range.days}
              key={range.days}
              onClick={() => setDays(range.days)}
            >
              {range.label}
            </button>
          ))}
        </div>
      </div>
      {error && (
        <div role="alert" className="inline-error">
          {error}
        </div>
      )}
      {loading ? (
        <div className="loading" role="status">
          Loading activity…
        </div>
      ) : (
        <>
          <section className="activity-overview" aria-label="Usage summary">
            {[
              {
                label: "Requests",
                value: count(totalRequests),
                detail: elapsedDays
                  ? `${count(totalRequests / elapsedDays)} / day`
                  : "all time",
              },
              {
                label: "Total tokens",
                value: count(totalTokens),
                detail: `${count(totalRequests ? totalTokens / totalRequests : 0)} / request`,
              },
              {
                label: "Failed",
                value: count(failedRequests),
                detail: `${percentage(failedRequests, totalRequests, 1)}${topStatus ? ` · top ${topStatus[0]}` : ""}`,
                tone: failedRequests ? "warn" : "",
              },
            ].map((stat) => (
              <div className="activity-stat" key={stat.label}>
                <span className="label">{stat.label}</span>
                <strong className={stat.tone ? `text-${stat.tone}` : undefined}>
                  {stat.value}
                </strong>
                <code>{stat.detail}</code>
              </div>
            ))}

            <section
              className="activity-value"
              aria-labelledby="api-value-title"
            >
              <div className="activity-section-heading">
                <h2 className="label" id="api-value-title">
                  Estimated API value
                </h2>
              </div>
              <strong>
                {groupAmount(
                  totals?.usd ?? 0,
                  totals?.pricedRequests ?? 0,
                  totals?.requests ?? 0,
                )}
              </strong>
              <details className="pricing-details">
                <summary>Pricing details · {currency}</summary>
                <div className="segmented" aria-label="Display currency">
                  {(["USD", "EUR"] as const).map((unit) => (
                    <button
                      type="button"
                      key={unit}
                      aria-pressed={currency === unit}
                      className={currency === unit ? "active" : ""}
                      onClick={() => setCurrency(unit)}
                    >
                      {unit === "USD" ? "$ USD" : "€ EUR"}
                    </button>
                  ))}
                </div>
                <p className="activity-note">
                  Full selected period · {count(totals?.pricedRequests ?? 0)} of{" "}
                  {count(totals?.requests ?? 0)} requests priced.
                  {totals &&
                    totals.pricedRequests < totals.requests &&
                    " Missing prices or token breakdowns are excluded, not counted as free."}
                  {pricing?.refreshing && " Refreshing list prices…"}
                  {pricing?.fetchedAt &&
                    ` Prices updated ${dateLabel(pricing.fetchedAt, rowTimestamp)}.`}
                  {currency === "EUR" &&
                    (pricing?.eur
                      ? ` EUR rate dated ${pricing.eur.date}.`
                      : " EUR exchange rate unavailable.")}
                </p>
              </details>
            </section>
          </section>

          {records.length > 0 && (
            <section
              className="activity-section requests-chart"
              aria-labelledby="requests-title"
            >
              <div className="activity-section-heading">
                <h2 className="label" id="requests-title">
                  Request activity
                </h2>
                <code>
                  {count(records.length)} sampled requests · per{" "}
                  {days === 1 ? "hour" : "day"}
                </code>
              </div>
              <div className="chart-plot">
                {buckets.map((bucket) => {
                  const height = (bucket.total / maxBucket) * 100;
                  const failedHeight = bucket.total
                    ? (bucket.failed / bucket.total) * 100
                    : 0;
                  return (
                    <div
                      className="chart-column"
                      key={bucket.key}
                      title={`${bucket.accessibleLabel}: ${bucket.total} requests, ${bucket.failed} failed`}
                    >
                      <div
                        className={`chart-bar ${bucket.total ? "" : "empty"}`}
                        style={{ height: `${height}%` }}
                      >
                        <span
                          className="chart-failed"
                          style={{ height: `${failedHeight}%` }}
                        />
                      </div>
                      <code>{bucket.showLabel ? bucket.label : ""}</code>
                    </div>
                  );
                })}
              </div>
            </section>
          )}

          <ModelTokenChart models={byModel} requests={records.length} />

          <details
            className="activity-disclosure token-mix"
            aria-labelledby="token-mix-title"
          >
            <summary id="token-mix-title">
              Token details <span>Cache, output & latency</span>
            </summary>
            <div className="activity-secondary-stats">
              {[
                {
                  label: "Cache hit",
                  value: cacheRate === null ? "—" : `${cacheRate.toFixed(0)}%`,
                  detail: completeTokens
                    ? `${count(tokens?.cached ?? 0)} cached`
                    : "incomplete token breakdown",
                },
                {
                  label: "p95 latency",
                  value: `${(p95Latency / 1000).toFixed(2)}s`,
                  detail: `latest ${count(records.length)} reqs`,
                },
                {
                  label: "Avg output",
                  value: completeTokens
                    ? count(
                        totalRequests
                          ? (tokens?.output ?? 0) / totalRequests
                          : 0,
                      )
                    : "—",
                  detail: "tokens / request",
                },
              ].map((stat) => (
                <div className="activity-stat" key={stat.label}>
                  <span className="label">{stat.label}</span>
                  <strong>{stat.value}</strong>
                  <code>{stat.detail}</code>
                </div>
              ))}
            </div>
            <div
              className="token-mix-bar"
              role="img"
              aria-label={tokenParts
                .map((part) => `${part.label} ${count(part.value)}`)
                .join(", ")}
            >
              {tokenParts.map((part) => (
                <span
                  className={part.className}
                  key={part.label}
                  style={{ width: `${percentage(part.value, tokenMixTotal)}` }}
                />
              ))}
            </div>
            <div className="token-legend">
              {tokenParts.map((part) => (
                <span className={part.className} key={part.label}>
                  <i aria-hidden="true" /> {part.label} {count(part.value)}
                </span>
              ))}
            </div>
          </details>

          {!records.length ? (
            <Empty title="Ready for your first request">
              Connect an agent to this profile. Requests, token counts, and
              timing will appear here.
            </Empty>
          ) : (
            <>
              <details
                className="activity-disclosure"
                aria-labelledby="accounts-title"
              >
                <summary id="accounts-title">
                  By account <span>Request share & reliability</span>
                </summary>
                <div className="table-scroll">
                  <table className="data-table account-activity-table">
                    <thead>
                      <tr>
                        <th>Account</th>
                        <th>Share</th>
                        <th className="num">Reqs</th>
                        <th className="num">API value ({currency})</th>
                        <th className="num">Avg latency</th>
                        <th className="num">Err</th>
                      </tr>
                    </thead>
                    <tbody>
                      {byAccount.map((group) => (
                        <tr key={group.label}>
                          <td>{group.label}</td>
                          <td>
                            <div className="account-share">
                              <div className="meter-track">
                                <div
                                  style={{
                                    width: percentage(
                                      group.requests,
                                      records.length,
                                    ),
                                  }}
                                />
                              </div>
                              <span>
                                {percentage(group.requests, records.length)}
                              </span>
                            </div>
                          </td>
                          <td className="num">{count(group.requests)}</td>
                          <td className="num">
                            {groupAmount(
                              group.usd,
                              group.priced,
                              group.requests,
                            )}
                          </td>
                          <td className="num">
                            {(group.latency / group.requests / 1000).toFixed(2)}
                            s
                          </td>
                          <td
                            className={`num ${group.failed ? "text-warn" : ""}`}
                          >
                            {percentage(group.failed, group.requests, 1)}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </details>

              <details
                className="activity-disclosure"
                aria-labelledby="models-title"
              >
                <summary id="models-title">
                  By model <span>Tokens, value & latency</span>
                </summary>
                <div className="table-scroll">
                  <table className="data-table">
                    <thead>
                      <tr>
                        <th>Model</th>
                        <th className="num">Reqs</th>
                        <th className="num">Tokens</th>
                        <th className="num">API value ({currency})</th>
                        <th className="num">Err</th>
                        <th className="num">p50 latency</th>
                      </tr>
                    </thead>
                    <tbody>
                      {byModel.map(([model, group]) => (
                        <tr key={model}>
                          <td>{model}</td>
                          <td className="num">{count(group.requests)}</td>
                          <td className="num">{count(group.tokens)}</td>
                          <td className="num">
                            {groupAmount(
                              group.usd,
                              group.priced,
                              group.requests,
                            )}
                          </td>
                          <td
                            className={`num ${group.failed ? "text-warn" : ""}`}
                          >
                            {percentage(group.failed, group.requests, 1)}
                          </td>
                          <td className="num">
                            {(percentile(group.latencies, 0.5) / 1000).toFixed(
                              2,
                            )}
                            s
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </details>

              <section
                className="activity-section request-log"
                aria-labelledby="request-log-title"
              >
                <div className="activity-section-heading request-log-heading">
                  <h2 className="label" id="request-log-title">
                    Request log
                  </h2>
                </div>
                <div className="activity-toolbar">
                  <label className="search-field">
                    <Search size={14} aria-hidden="true" />
                    <input
                      placeholder="Filter model, account, or email"
                      aria-label="Search activity"
                      value={query}
                      onChange={(event) => setQuery(event.target.value)}
                    />
                  </label>
                  <button
                    type="button"
                    className={`button small ${failures ? "active" : ""}`}
                    aria-pressed={failures}
                    onClick={() => setFailures((current) => !current)}
                  >
                    Errors only
                  </button>
                  <button
                    className="button small export-activity"
                    onClick={exportCSV}
                    disabled={!filtered.length}
                  >
                    <Download size={14} /> Export
                  </button>
                </div>
                {!filtered.length ? (
                  <Empty title="No matching requests">
                    Try a model, account, or email, or turn off the error
                    filter.
                  </Empty>
                ) : (
                  <div className="table-scroll">
                    <table className="data-table request-table">
                      <thead>
                        <tr>
                          <th>Time</th>
                          <th>Model</th>
                          <th>Account</th>
                          <th className="num">Tokens</th>
                          <th className="num">API value ({currency})</th>
                          <th className="num">Lat ms</th>
                          <th className="num">Code</th>
                        </tr>
                      </thead>
                      <tbody>
                        {filtered.map((record) => (
                          <tr key={record.id}>
                            <td
                              title={dateLabel(record.timestamp, rowTimestamp)}
                            >
                              {dateLabel(record.timestamp, rowTime)}
                            </td>
                            <td>
                              {record.model}
                              {record.stream && (
                                <small className="stream-label">
                                  {" "}
                                  · stream
                                </small>
                              )}
                            </td>
                            <td>{recordAccount(record)}</td>
                            <td className="num">{count(record.totalTokens)}</td>
                            <td className="num">{amount(costOf(record))}</td>
                            <td className="num">{count(record.latencyMs)}</td>
                            <td
                              className={`num ${
                                record.statusCode >= 500
                                  ? "text-bad"
                                  : record.statusCode === 429
                                    ? "text-warn"
                                    : ""
                              }`}
                            >
                              {record.statusCode}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </section>
            </>
          )}
          <p className="activity-note">
            Token counts come from the proxy; they are not a subscription bill.
            {records.length >= 500 && (
              <>
                {" "}
                Charts, tables, and CSV use the latest 500 requests; summary
                totals and estimated API value include the full range.
              </>
            )}
          </p>
        </>
      )}
    </div>
  );
}
