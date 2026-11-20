import type { UsageRecord } from "../shared/types.ts";
import { allHistoryDays } from "../shared/vocabulary.ts";

export function activityBuckets(
  records: UsageRecord[],
  days: number,
  now = new Date(),
) {
  const hourly = days === 1;
  const cutoff =
    days === allHistoryDays ? -Infinity : now.getTime() - days * 86_400_000;
  const dated = records
    .map((record) => ({ record, time: new Date(record.timestamp).getTime() }))
    .filter(
      ({ time }) =>
        Number.isFinite(time) && time >= cutoff && time <= now.getTime(),
    );
  const start = new Date(
    days === allHistoryDays
      ? dated.reduce(
          (oldest, { time }) => Math.min(oldest, time),
          now.getTime(),
        )
      : cutoff,
  );
  if (hourly) start.setMinutes(0, 0, 0);
  else start.setHours(0, 0, 0, 0);
  const label = new Intl.DateTimeFormat(
    [],
    hourly
      ? { hour: "numeric" }
      : { month: days > 8 ? "numeric" : "short", day: "numeric" },
  );
  const accessibleLabel = new Intl.DateTimeFormat(
    [],
    hourly
      ? {
          month: "short",
          day: "numeric",
          hour: "numeric",
          timeZoneName: "short",
        }
      : { month: "long", day: "numeric", year: "numeric" },
  );
  const buckets = [];
  for (let date = new Date(start); date <= now;) {
    buckets.push({
      key: date.getTime(),
      label: label.format(date),
      accessibleLabel: accessibleLabel.format(date),
      total: 0,
      failed: 0,
      showLabel: false,
    });
    if (hourly) date = new Date(date.getTime() + 3_600_000);
    else date.setDate(date.getDate() + 1);
  }
  const byDay = new Map(buckets.map((bucket) => [bucket.key, bucket]));
  for (const { record, time } of dated) {
    const date = new Date(time);
    date.setHours(0, 0, 0, 0);
    const bucket = hourly
      ? buckets[Math.floor((time - start.getTime()) / 3_600_000)]
      : byDay.get(date.getTime());
    if (bucket) {
      bucket.total++;
      bucket.failed += Number(record.failed);
    }
  }
  const every = Math.max(1, Math.ceil(buckets.length / 7));
  buckets.forEach((bucket, index) => {
    bucket.showLabel = index % every === 0 || index === buckets.length - 1;
  });
  return buckets;
}
