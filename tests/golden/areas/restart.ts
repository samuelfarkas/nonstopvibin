// restart-policy.ts: event sequences through transition() with its own clock argument.
// Each case is one sequence: the initial facts, the timing, then each event with its
// `now`; the output is each step's state, actions and wakeAt. ±Infinity, which JSON
// cannot hold, is written as the strings "Infinity" and "-Infinity".
import {
  giveUpText,
  initial,
  type RestartEvent,
  type RestartState,
  type RestartTiming,
  transition,
  wakeAt,
} from "../../../src/server/restart-policy.ts";
import type { Case, Recorded } from "../lib.ts";

type Facts = Parameters<typeof initial>[0];
interface Sequence {
  facts: Facts;
  timing: RestartTiming;
  events: Array<{ event: RestartEvent; now: number }>;
}

const timing: RestartTiming = {
  coalesce: 1000,
  restartBackoff: 60_000,
  restartLimit: 5,
};
const finite = (value: Recorded): Recorded =>
  JSON.parse(
    JSON.stringify(value, (_, v: Recorded) =>
      v === Infinity ? "Infinity" : v === -Infinity ? "-Infinity" : v,
    ),
  );

function run(sequence: Sequence) {
  let state: RestartState = initial(sequence.facts);
  const steps: Recorded[] = [
    { initial: state, wakeAt: wakeAt(state, sequence.timing) },
  ];
  for (const { event, now } of sequence.events) {
    const step = transition(state, event, now, sequence.timing);
    state = step.state;
    steps.push({
      state: step.state,
      actions: step.actions,
      wakeAt: wakeAt(step.state, sequence.timing),
    });
  }
  return finite(steps);
}

const fresh: Facts = {
  running: false,
  reported: false,
  kept: true,
  starting: false,
  holding: false,
};
const at = (now: number, event: RestartEvent) => ({ event, now });
const autoStart = (now: number, running: boolean, error?: string) => [
  at(now, { type: "starting", automatic: true }),
  at(now + 10, {
    type: "started",
    automatic: true,
    running,
    ...(error !== undefined && { error }),
  }),
];

const named = {
  "first start succeeds": {
    facts: fresh,
    timing,
    events: [at(0, { type: "time" }), ...autoStart(0, true)],
  },
  "running core stays idle": {
    facts: { ...fresh, running: true },
    timing,
    events: [at(0, { type: "time" })],
  },
  "reported only counts while not running": {
    facts: { ...fresh, running: true, reported: true },
    timing,
    events: [
      at(0, { type: "exited", error: "crash" }),
      at(1, { type: "time" }),
    ],
  },
  "backoff doubles to the limit then gives up": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, false, "boom"),
      at(1010, { type: "time" }),
      ...autoStart(1010, false, "boom"),
      at(3020, { type: "time" }),
      ...autoStart(3020, false, "boom"),
      at(7030, { type: "time" }),
      ...autoStart(7030, false, "boom"),
      at(15_040, { type: "time" }),
      ...autoStart(15_040, false, "boom"),
      at(31_050, { type: "time" }),
      at(99_999, { type: "time" }),
    ],
  },
  "early time does nothing": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, false, "x"),
      at(500, { type: "time" }),
    ],
  },
  "core exits soon after starting": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, true),
      at(5000, { type: "exited", error: "died" }),
      at(6010, { type: "time" }),
    ],
  },
  "core up for the backoff ends the run": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, false, "x"),
      at(1010, { type: "time" }),
      ...autoStart(1010, true),
      at(61_020, { type: "exited", error: "later" }),
      at(61_021, { type: "time" }),
    ],
  },
  "exit without error is quiet": {
    facts: { ...fresh, running: true },
    timing,
    events: [at(0, { type: "exited" }), at(0, { type: "time" })],
  },
  "manual start ends a given-up run": {
    facts: fresh,
    timing: { coalesce: 10, restartBackoff: 100, restartLimit: 1 },
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, false, "x"),
      at(20, { type: "time" }),
      at(30, { type: "starting", automatic: false }),
      at(40, { type: "started", automatic: false, running: true }),
    ],
  },
  "manual start that fails keeps the run": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "starting", automatic: false }),
      at(1, { type: "time" }),
      at(2, {
        type: "started",
        automatic: false,
        running: false,
        error: "manual",
      }),
      at(3, { type: "time" }),
    ],
  },
  "holding blocks starts": {
    facts: fresh,
    timing,
    events: [
      at(0, { type: "holding", holding: true }),
      at(1, { type: "time" }),
      at(2, { type: "holding", holding: false }),
      at(3, { type: "time" }),
    ],
  },
  "not kept waits for nothing": {
    facts: { ...fresh, kept: false },
    timing,
    events: [
      at(0, { type: "time" }),
      at(1, { type: "kept", kept: true }),
      at(2, { type: "kept", kept: false }),
    ],
  },
  "already reported error is not reported again": {
    facts: { ...fresh, reported: true },
    timing,
    events: [
      at(0, { type: "time" }),
      ...autoStart(0, false, "same"),
      at(1010, { type: "time" }),
      ...autoStart(1010, false, "same"),
    ],
  },
  "automatic started without starting is ignored": {
    facts: fresh,
    timing,
    events: [at(0, { type: "started", automatic: true, running: true })],
  },
  "failure without error text": {
    facts: fresh,
    timing,
    events: [at(0, { type: "time" }), ...autoStart(0, false)],
  },
  "give up text": {
    facts: fresh,
    timing: { coalesce: 1, restartBackoff: 1, restartLimit: 0 },
    events: [at(0, { type: "time" })],
  },
} satisfies Record<string, Sequence>;

/** A small deterministic PRNG, so the random corpus is the same on every run. */
function mulberry32(seed: number) {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function randomSequence(seed: number): Sequence {
  const random = mulberry32(seed);
  const pick = <T>(items: T[]): T => items[Math.floor(random() * items.length)];
  const bool = () => random() < 0.5;
  const facts: Facts = {
    running: random() < 0.3,
    reported: random() < 0.3,
    kept: random() < 0.85,
    starting: random() < 0.1,
    holding: random() < 0.1,
  };
  const t: RestartTiming = pick([
    timing,
    { coalesce: 5, restartBackoff: 40, restartLimit: 3 },
    { coalesce: 100, restartBackoff: 150, restartLimit: 2 },
  ]);
  const events: Sequence["events"] = [];
  let now = Math.floor(random() * 50);
  for (let i = 0; i < 14; i++) {
    now += pick([
      0,
      1,
      t.coalesce,
      t.coalesce * 2,
      t.restartBackoff,
      Math.floor(random() * t.restartBackoff * 2),
    ]);
    const event: RestartEvent = pick<RestartEvent>([
      { type: "time" },
      { type: "time" },
      { type: "time" },
      { type: "starting", automatic: true },
      { type: "started", automatic: true, running: true },
      {
        type: "started",
        automatic: true,
        running: false,
        error: "start failed",
      },
      { type: "started", automatic: true, running: false },
      { type: "starting", automatic: false },
      { type: "started", automatic: false, running: bool() },
      { type: "exited", error: "core exited" },
      { type: "exited" },
      { type: "kept", kept: random() < 0.8 },
      { type: "holding", holding: bool() },
    ]);
    events.push({ event, now });
  }
  return { facts, timing: t, events };
}

/**
 * A sequence that follows the policy as the scheduler does: it starts when told, wakes
 * when asked, and sometimes something else happens; starts fail at random.
 */
function drivenSequence(seed: number): Sequence {
  const random = mulberry32(seed * 7919);
  const t: RestartTiming =
    seed % 2 ? timing : { coalesce: 5, restartBackoff: 40, restartLimit: 3 };
  const facts: Facts = {
    ...fresh,
    running: random() < 0.3,
    reported: random() < 0.2,
  };
  let state = initial(facts);
  let now = 0;
  const events: Sequence["events"] = [];
  const emit = (event: RestartEvent) => {
    events.push({ event, now });
    const step = transition(state, event, now, t);
    state = step.state;
    return step.actions;
  };
  let actions = emit({ type: "time" });
  while (events.length < 16) {
    const roll = random();
    if (actions.some((a) => a.type === "start")) {
      emit({ type: "starting", automatic: true });
      now += 10;
      const running = random() < 0.35;
      actions = emit({
        type: "started",
        automatic: true,
        running,
        ...(!running && random() < 0.8 && { error: `failure ${seed}` }),
      });
    } else if (roll < 0.12) {
      now += Math.floor(random() * t.restartBackoff);
      actions = emit({
        type: "exited",
        ...(random() < 0.7 && { error: "core exited" }),
      });
    } else if (roll < 0.17) {
      actions = emit({ type: "holding", holding: !state.holding });
    } else if (roll < 0.22) {
      emit({ type: "starting", automatic: false });
      now += 5;
      actions = emit({
        type: "started",
        automatic: false,
        running: random() < 0.6,
      });
    } else {
      const wake = actions.find((a) => a.type === "wake");
      now =
        wake && wake.at > now
          ? wake.at
          : now + Math.floor(random() * t.restartBackoff * 1.5);
      actions = emit({ type: "time" });
    }
  }
  return { facts, timing: t, events };
}

export function recordRestart(): Promise<Case[]> {
  const cases: Case[] = Object.entries(named).map(([name, sequence]) => ({
    name,
    input: sequence,
    output: run(sequence),
  }));
  for (let seed = 1; seed <= 60; seed++) {
    const sequence = randomSequence(seed);
    cases.push({
      name: `random seed ${seed}`,
      input: sequence,
      output: run(sequence),
    });
  }
  for (let seed = 1; seed <= 60; seed++) {
    const sequence = drivenSequence(seed);
    cases.push({
      name: `driven seed ${seed}`,
      input: sequence,
      output: run(sequence),
    });
  }
  cases.push({
    name: "giveUpText",
    input: [0, 1, 5],
    output: [0, 1, 5].map(giveUpText),
  });
  return Promise.resolve(cases);
}
