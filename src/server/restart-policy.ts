/**
 * When the scheduler starts a profile the app keeps running (it has a subscription): one
 * profile's state, the events that change it and what to do next. Pure: no timers, pool or
 * store, and time is an argument, so a decision depends only on what happened, in order, and when.
 */

/** The restart periods of the scheduler's timing, in milliseconds. */
export interface RestartTiming {
  /** Shortest wait between scheduler wakeups; the wait after the first failed start. */
  coalesce: number;
  /** Longest wait between automatic starts, and how long a core must stay up to end a run of failures. */
  restartBackoff: number;
  /** Automatic starts in one run of failures before the scheduler stops starting the profile. */
  restartLimit: number;
}

/**
 * Where the profile is in a run of failures: automatic starts in a row that failed, or whose
 * core exited before it had been up for `restartBackoff` since its start ended. `reported`:
 * the run already has its error-list entry.
 */
export type Run =
  /** No run. `running`: its core runs. `reported`: the launch restore reported its failed start. */
  | { phase: "idle"; running: boolean; reported: boolean }
  /** Its core is down after `attempts` automatic starts; the next may begin at `due`. */
  | { phase: "waiting"; attempts: number; due: number; reported: boolean }
  /** Automatic start number `attempts` is in flight. */
  | { phase: "starting"; attempts: number; reported: boolean }
  /** That start ended at `since` with its core running. */
  | { phase: "up"; attempts: number; since: number; reported: boolean }
  /** The scheduler stopped starting it after `attempts` automatic starts. */
  | { phase: "gave-up"; attempts: number };

export interface RestartState {
  run: Run;
  /** It has a subscription and exists. */
  kept: boolean;
  /** A start by someone else is in flight. */
  starting: boolean;
  /** A stop or a credential swap is in flight. */
  holding: boolean;
}

/** What can happen to a profile, told as it happens. */
export type RestartEvent =
  /** It gained or lost its last subscription, or no longer exists. */
  | { type: "kept"; kept: boolean }
  /** A stop or credential swap began, or none is in flight any more. */
  | { type: "holding"; holding: boolean }
  /** A start began; `automatic`: the scheduler's, which this policy began itself. */
  | { type: "starting"; automatic: boolean }
  /** That start ended; `running`: with its core running; `error`: why not. */
  | { type: "started"; automatic: boolean; running: boolean; error?: string }
  /** Its core went away; `error` unless it was stopped on purpose. */
  | { type: "exited"; error?: string }
  /** A wakeup this policy asked for came due. */
  | { type: "time" };

export type Action =
  | { type: "start" }
  /** Call again with `time` at `at`; a step without one wants no wakeup. -Infinity: at once. */
  | { type: "wake"; at: number }
  /** One error-list entry. */
  | { type: "report"; text: string }
  /** What the profile shows instead of its last failure, or nothing. */
  | { type: "show"; text: string | undefined };

export interface Step {
  state: RestartState;
  actions: Action[];
}

export function initial(facts: {
  running: boolean;
  reported: boolean;
  kept: boolean;
  starting: boolean;
  holding: boolean;
}): RestartState {
  const { running, reported, kept, starting, holding } = facts;
  return {
    run: { phase: "idle", running, reported: reported && !running },
    kept,
    starting,
    holding,
  };
}

export const giveUpText = (attempts: number) =>
  `Stopped restarting after ${attempts} failed starts in a row. Use Restart to try again.`;

/** When the profile wants `time`; Infinity while it cannot or need not be started. */
export function wakeAt(state: RestartState, timing: RestartTiming): number {
  const { run } = state;
  if (!state.kept || state.starting || state.holding) return Infinity;
  if (run.phase === "idle") return run.running ? Infinity : -Infinity;
  if (run.phase === "waiting")
    // At the limit the next step is giving up, which waits for nothing.
    return run.attempts >= timing.restartLimit ? -Infinity : run.due;
  return Infinity;
}

export function transition(
  state: RestartState,
  event: RestartEvent,
  now: number,
  timing: RestartTiming,
): Step {
  const actions: Action[] = [];
  let { run, kept, starting, holding } = state;
  // A core up for the whole bound ended the run, whether or not anything looked meanwhile.
  if (run.phase === "up" && now - run.since >= timing.restartBackoff)
    run = { phase: "idle", running: true, reported: false };
  /** Automatic start `attempts`, which ended at `ended`, failed or lost its core too soon. */
  const failed = (
    attempts: number,
    ended: number,
    reported: boolean,
    error: string | undefined,
  ): Run => {
    if (error !== undefined && !reported)
      actions.push({ type: "report", text: error });
    return {
      phase: "waiting",
      attempts,
      due:
        ended +
        Math.min(timing.coalesce * 2 ** (attempts - 1), timing.restartBackoff),
      reported: reported || error !== undefined,
    };
  };
  switch (event.type) {
    case "kept":
      kept = event.kept;
      break;
    case "holding":
      holding = event.holding;
      break;
    case "starting":
      if (!event.automatic) starting = true;
      break;
    case "started":
      if (!event.automatic) {
        starting = false;
        // Someone else's start left a core running: the run ends, given up or not.
        if (event.running) {
          if (run.phase === "gave-up")
            actions.push({ type: "show", text: undefined });
          run = { phase: "idle", running: true, reported: false };
        }
      } else if (run.phase === "starting")
        run = event.running
          ? {
              phase: "up",
              attempts: run.attempts,
              since: now,
              reported: run.reported,
            }
          : failed(run.attempts, now, run.reported, event.error);
      break;
    case "exited":
      // While a start is in flight its own end says whether a core runs.
      if (run.phase === "idle" && run.running) run = { ...run, running: false };
      else if (run.phase === "up")
        run = failed(run.attempts, run.since, run.reported, event.error);
      break;
    case "time":
      if (wakeAt({ run, kept, starting, holding }, timing) > now) break;
      if (run.phase === "waiting" && run.attempts >= timing.restartLimit) {
        const text = giveUpText(run.attempts);
        actions.push({ type: "report", text }, { type: "show", text });
        run = { phase: "gave-up", attempts: run.attempts };
      } else if (run.phase === "idle" || run.phase === "waiting") {
        run = {
          phase: "starting",
          attempts: run.phase === "idle" ? 1 : run.attempts + 1,
          reported: run.reported,
        };
        actions.push({ type: "start" });
      }
  }
  const next = { run, kept, starting, holding };
  const at = wakeAt(next, timing);
  if (at < Infinity) actions.push({ type: "wake", at });
  return { state: next, actions };
}
