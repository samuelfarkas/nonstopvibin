import type { CorePool } from "../src/server/core.ts";

/**
 * Awaits a teardown step but fails after `limit` ms naming the cores it is still
 * waiting for. Set `limit` below the hook's own, whose timeout names nothing.
 */
export async function settle(
  step: string,
  operation: Promise<unknown>,
  core: CorePool,
  limit: number,
): Promise<void> {
  let timer: NodeJS.Timeout | undefined;
  const expired = new Promise<never>((_, reject) => {
    timer = setTimeout(
      () =>
        reject(
          new Error(
            `${step} was still waiting after ${limit} ms: ${pending(core)}`,
          ),
        ),
      limit,
    );
  });
  try {
    await Promise.race([operation, expired]);
  } finally {
    clearTimeout(timer);
  }
}

function pending(core: CorePool): string {
  const alive = [...core.runtimes].filter(([id]) => core.alive(id));
  if (!alive.length) return "every core has exited, so not on a core.";
  return alive
    .map(([id, runtime]) => {
      let name = id;
      try {
        name = core.store.profile(id).name;
      } catch {
        // A deleted profile or a closed store leaves the ID.
      }
      const phase = runtime.child.killed
        ? "told to exit, waiting for it to"
        : "not told to exit yet (usage drain, or an earlier shutdown step)";
      return `${name} core pid ${runtime.child.pid} (${runtime.state}${runtime.stopping ? ", stopping" : ""}): ${phase}`;
    })
    .join("; ");
}
