// The restart policy on its own: named sequences from the review rounds, then every state
// reachable from every starting point, with the invariants checked on each transition.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  giveUpText,
  initial,
  transition,
  wakeAt,
  type Action,
  type RestartEvent,
  type RestartState,
  type RestartTiming,
} from "../src/server/restart-policy.ts";
import { usageTiming } from "../src/server/usage.ts";

const small: RestartTiming = {
  coalesce: 1,
  restartBackoff: 4,
  restartLimit: 3,
};
const fresh = (overrides: Partial<Parameters<typeof initial>[0]> = {}) =>
  initial({
    running: false,
    reported: false,
    kept: true,
    starting: false,
    holding: false,
    ...overrides,
  });

/** Feeds `[event, now]` pairs in order; answers the last state and every action, in order. */
function drive(
  state: RestartState,
  events: [RestartEvent, number][],
  timing = small,
) {
  const actions: Action[] = [];
  for (const [event, now] of events) {
    const step = transition(state, event, now, timing);
    state = step.state;
    actions.push(...step.actions);
  }
  return { state, actions };
}
const time: RestartEvent = { type: "time" };
const autoOk: RestartEvent = {
  type: "started",
  automatic: true,
  running: true,
};
const autoFailed: RestartEvent = {
  type: "started",
  automatic: true,
  running: false,
  error: "failed",
};
const crash: RestartEvent = { type: "exited", error: "crashed" };
const reports = (actions: Action[]) =>
  actions.flatMap((a) => (a.type === "report" ? [a.text] : []));
const starts = (actions: Action[]) =>
  actions.filter((a) => a.type === "start").length;

/** Up to the limit's last automatic start, whose core is now running since `since`. */
function lastAttemptUp() {
  // Starts at 0, 2 and 5: each failed start waits 1, then 2 before the next.
  const { state } = drive(fresh(), [
    [time, 0],
    [autoFailed, 1],
    [time, 2],
    [autoFailed, 3],
    [time, 5],
    [autoOk, 6],
  ]);
  assert.deepEqual(state.run, {
    phase: "up",
    attempts: 3,
    since: 6,
    reported: true,
  });
  return { state, since: 6 };
}

test("the limit's last core, up past the bound with nothing looking, exits: a fresh run, not a give-up", () => {
  const { state, since } = lastAttemptUp();
  // No event at all between its start and its exit just past the bound.
  const after = drive(state, [
    [crash, since + small.restartBackoff],
    [time, since + small.restartBackoff],
  ]);
  assert.deepEqual(reports(after.actions), []);
  assert.equal(after.state.run.phase, "starting");
  assert.deepEqual(after.state.run, {
    phase: "starting",
    attempts: 1,
    reported: false,
  });
  // Just under the bound it was one more failure, and the limit is reached.
  const before = drive(state, [
    [crash, since + small.restartBackoff - 1],
    [time, since + small.restartBackoff - 1],
  ]);
  assert.equal(before.state.run.phase, "gave-up");
  assert.deepEqual(reports(before.actions), [giveUpText(3)]);
});

test("someone else's start that leaves a core running ends the run, whatever the scheduler still does", () => {
  const { state, since } = lastAttemptUp();
  // The scheduler's quota check after that start may still be pending: the policy does not know it.
  const { state: next, actions } = drive(state, [
    [crash, since + 1],
    // The user's Start begins before any wakeup could give up.
    [{ type: "starting", automatic: false }, since + 1],
    [{ type: "started", automatic: false, running: true }, since + 2],
    [crash, since + 3],
    [time, since + 3],
  ]);
  assert.deepEqual(next.run, {
    phase: "starting",
    attempts: 1,
    reported: false,
  });
  assert.ok(!reports(actions).includes(giveUpText(3)));
  // Without it, the same exit gives up at the next wakeup.
  const alone = drive(state, [
    [crash, since + 1],
    [time, since + 1],
  ]);
  assert.equal(alone.state.run.phase, "gave-up");
});

test("a start in flight is decided by its own end, whatever time passes or the core does meanwhile", () => {
  const { state } = drive(fresh(), [[time, 0]]);
  assert.equal(state.run.phase, "starting");
  // The core answered and counts as running, then exits during readiness; ticks come and go.
  const during = drive(state, [
    [time, 1],
    [crash, 2],
    [time, 100],
  ]);
  assert.deepEqual(during.state, state);
  assert.deepEqual(during.actions, []);
  // The backoff counts from the start's end, and the run keeps its count.
  const failed = drive(during.state, [[autoFailed, 150]]);
  assert.deepEqual(failed.state.run, {
    phase: "waiting",
    attempts: 1,
    due: 151,
    reported: true,
  });
});

test("only a failure the launch reported counts as this run's entry", () => {
  const launch = drive(fresh({ reported: true }), [
    [time, 0],
    [autoFailed, 1],
  ]);
  assert.deepEqual(reports(launch.actions), []);
  // Started by the launch, then crashed: nothing reported it yet.
  const crashed = drive(fresh({ running: true }), [
    [crash, 0],
    [time, 0],
    [autoFailed, 1],
    [time, 2],
    [autoFailed, 3],
  ]);
  assert.deepEqual(reports(crashed.actions), ["failed"]);
});

test("a blocked profile asks for no wakeup, and asks again once the obstacle ends", () => {
  for (const blocked of [
    { type: "holding", holding: true },
    { type: "starting", automatic: false },
    { type: "kept", kept: false },
  ] as const) {
    const { state, actions } = drive(fresh(), [[blocked, 0]]);
    assert.ok(!actions.some((a) => a.type === "wake"), blocked.type);
    assert.equal(wakeAt(state, small), Infinity);
    const ended = drive(state, [
      [
        blocked.type === "holding"
          ? { type: "holding", holding: false }
          : blocked.type === "kept"
            ? { type: "kept", kept: true }
            : { type: "started", automatic: false, running: false },
        5,
      ],
    ]);
    assert.deepEqual(ended.actions, [{ type: "wake", at: -Infinity }]);
  }
});

test("a failed start by someone else changes nothing, given up or mid-run", () => {
  const someoneFails: [RestartEvent, number][] = [
    [{ type: "starting", automatic: false }, 10],
    [{ type: "started", automatic: false, running: false, error: "no" }, 11],
  ];
  const { state: midRun } = drive(fresh(), [
    [time, 0],
    [autoFailed, 1],
    [time, 2],
    [autoFailed, 3],
  ]);
  assert.deepEqual(drive(midRun, someoneFails).state, midRun);
  const { state: gaveUp } = drive(midRun, [
    [time, 5],
    [autoFailed, 6],
    [time, 6],
  ]);
  assert.equal(gaveUp.run.phase, "gave-up");
  const after = drive(gaveUp, someoneFails);
  assert.deepEqual(after.state, gaveUp);
  assert.deepEqual(after.actions, []);
});

test("with the default timing, waits double from 2 s to 256 s, then 300 s, and the tenth failure gives up", () => {
  let state = fresh();
  let now = 0;
  const waits: number[] = [];
  const all: Action[] = [];
  for (;;) {
    const step = transition(state, time, now, usageTiming);
    state = step.state;
    all.push(...step.actions);
    if (state.run.phase === "gave-up") break;
    assert.equal(state.run.phase, "starting");
    const failed = transition(state, autoFailed, now + 1, usageTiming);
    state = failed.state;
    all.push(...failed.actions);
    const due = wakeAt(state, usageTiming);
    if (due === -Infinity) continue;
    waits.push(due - (now + 1));
    now = due;
  }
  assert.deepEqual(
    waits,
    [2, 4, 8, 16, 32, 64, 128, 256, 300].map((s) => s * 1000),
  );
  assert.equal(starts(all), 10);
  assert.deepEqual(reports(all), ["failed", giveUpText(10)]);
  assert.deepEqual(all.at(-1), { type: "show", text: giveUpText(10) });
});

// ---- Every reachable state ----

/** What really happens to the profile, which the policy only hears about through events. */
interface World {
  /** A core runs and no start is in flight. */
  core: boolean;
  op: "none" | "auto" | "foreign";
  /** The scheduler's core exited before its start ended. */
  lost: boolean;
  holding: boolean;
  kept: boolean;
  /** When the start of the running core ended, if the scheduler made it. */
  upSince: number | null;
}
/** The rules, kept apart from the policy: one run of failures as the specification defines it. */
interface Monitor {
  inRun: boolean;
  starts: number;
  gaveUp: number;
  reports: number;
  /** The launch reported this profile's failed start, and nothing ran since. */
  launch: boolean;
  /** This run's first failure was the launch's. */
  quiet: boolean;
}
interface Node {
  state: RestartState;
  world: World;
  monitor: Monitor;
  /** The moves that first reached it. */
  path: string;
  depth: number;
}
type Move = {
  name: string;
  dt?: number;
  event?: RestartEvent;
  world?: Partial<World>;
};

/** The moves possible from `world`; every event happens at now = 0, after an advance. */
function moves(world: World, timing: RestartTiming): Move[] {
  const list: Move[] = [
    { name: "advance", dt: 1 },
    { name: "advance-bound", dt: timing.restartBackoff + 1 },
    { name: "time", event: time },
    {
      name: "kept",
      event: { type: "kept", kept: !world.kept },
      world: { kept: !world.kept },
    },
  ];
  if (!world.holding)
    list.push({
      name: "hold",
      event: { type: "holding", holding: true },
      world: { holding: true },
    });
  else {
    list.push({
      name: "release",
      event: { type: "holding", holding: false },
      world: { holding: false },
    });
    if (world.core)
      list.push({
        name: "stopped",
        event: { type: "exited" },
        world: { core: false, upSince: null },
      });
  }
  if (world.core)
    list.push({
      name: "crash",
      event: crash,
      world: { core: false, upSince: null },
    });
  if (world.op === "auto") {
    if (!world.lost)
      list.push({ name: "auto-lost", event: crash, world: { lost: true } });
    list.push(
      {
        name: "auto-ok",
        event: {
          type: "started",
          automatic: true,
          running: !world.lost,
          ...(world.lost && { error: "crashed" }),
        },
        world: {
          op: "none",
          lost: false,
          core: !world.lost,
          upSince: world.lost ? null : 0,
        },
      },
      {
        name: "auto-fail",
        event: autoFailed,
        world: { op: "none", lost: false },
      },
    );
  }
  if (world.op === "none" && !world.core && !world.holding)
    list.push({
      name: "other-start",
      event: { type: "starting", automatic: false },
      world: { op: "foreign" },
    });
  if (world.op === "foreign")
    list.push(
      {
        name: "other-ok",
        event: { type: "started", automatic: false, running: true },
        world: { op: "none", core: true, upSince: null },
      },
      {
        name: "other-fail",
        event: {
          type: "started",
          automatic: false,
          running: false,
          error: "no",
        },
        world: { op: "none" },
      },
    );
  return list;
}

/** Moves the clock forward by `dt`: times are kept relative to now, and those long past are all alike. */
function advance(node: Node, dt: number, timing: RestartTiming): Node {
  const bound = timing.restartBackoff;
  const { run } = node.state;
  return {
    ...node,
    state: {
      ...node.state,
      run:
        run.phase === "waiting"
          ? { ...run, due: Math.max(run.due - dt, 0) }
          : run.phase === "up"
            ? { ...run, since: Math.max(run.since - dt, -bound) }
            : run,
    },
    world: {
      ...node.world,
      upSince:
        node.world.upSince === null
          ? null
          : Math.max(node.world.upSince - dt, -bound),
    },
  };
}

function check(condition: boolean, message: string, node: Node, move: string) {
  if (!condition) assert.fail(`${message} after ${node.path} ${move}`);
}

/** One move, the policy's answer to it, and every invariant on the way. */
function apply(node: Node, move: Move, timing: RestartTiming): Node {
  if (move.dt !== undefined)
    return {
      ...advance(node, move.dt, timing),
      path: `${node.path} ${move.name}`,
      depth: node.depth + 1,
    };
  const before = node.state;
  let world = { ...node.world };
  const monitor = { ...node.monitor };
  const endRun = () => {
    monitor.inRun = false;
    monitor.starts = monitor.gaveUp = monitor.reports = 0;
    monitor.launch = false;
  };
  // The scheduler's core has been up for the bound: that run is over, whether or not anyone looked.
  if (
    world.core &&
    world.upSince !== null &&
    -world.upSince >= timing.restartBackoff
  )
    endRun();
  if (move.name === "other-ok") endRun();
  world = { ...world, ...move.world };
  if (!move.event) throw new Error(`${move.name} has no event`);
  const { state, actions: answered } = transition(
    before,
    move.event,
    0,
    timing,
  );
  let actions = answered;
  if (actions.some((a) => a.type === "start")) {
    check(
      !node.world.core &&
        node.world.op === "none" &&
        !node.world.holding &&
        node.world.kept &&
        move.name === "time" &&
        before.run.phase !== "gave-up",
      "started a profile that runs, is busy, held, without a subscription or given up",
      node,
      move.name,
    );
    if (!monitor.inRun) {
      monitor.inRun = true;
      monitor.quiet = monitor.launch;
    }
    monitor.starts++;
    world.op = "auto";
    // The pool says so from inside the start call.
    const began = transition(
      state,
      { type: "starting", automatic: true },
      0,
      timing,
    );
    check(
      JSON.stringify(began.state) === JSON.stringify(state),
      "its own start's notice changed the state",
      node,
      move.name,
    );
    actions = [...actions, ...began.actions.filter((a) => a.type !== "wake")];
  }
  const giveUps = reports(actions).filter((text) =>
    text.startsWith("Stopped"),
  ).length;
  monitor.gaveUp += giveUps;
  monitor.reports += reports(actions).length - giveUps;
  const next: Node = {
    state,
    world,
    monitor,
    path: `${node.path} ${move.name}`,
    depth: node.depth + 1,
  };
  const { run } = state;
  check(
    monitor.starts <= timing.restartLimit,
    "more starts than the limit in one run",
    next,
    "",
  );
  check(
    (run.phase !== "idle") === monitor.inRun,
    "the run ended at the wrong moment",
    next,
    "",
  );
  check(monitor.gaveUp <= 1, "gave up twice in one run", next, "");
  check(
    giveUps ===
      (before.run.phase !== "gave-up" && run.phase === "gave-up" ? 1 : 0),
    "the give-up line came without giving up, or giving up without it",
    next,
    "",
  );
  if (before.run.phase === "gave-up" && run.phase !== "gave-up")
    check(
      move.name === "other-ok",
      "left the give-up without someone else's start",
      next,
      "",
    );
  check(
    monitor.reports <= (monitor.quiet ? 0 : 1),
    "reported a run twice",
    next,
    "",
  );
  const wake = wakeAt(state, timing);
  const wakes = actions.filter((a) => a.type === "wake");
  check(
    wakes.length === (wake < Infinity ? 1 : 0) &&
      (wake === Infinity || wakes[0]!.at === wake),
    "the wakeup asked for is not the state's",
    next,
    "",
  );
  if (move.name === "time")
    check(wake > 0, "due again at once without a change: a spin", next, "");
  const free =
    world.kept && world.op === "none" && !world.holding && !world.core;
  check(
    wake < Infinity === (free && run.phase !== "gave-up"),
    "a wakeup while it cannot start, or none while it should",
    next,
    "",
  );
  // What the policy believes is what happened.
  check(
    (run.phase === "up" || (run.phase === "idle" && run.running)) ===
      world.core,
    "believes the wrong thing about its core",
    next,
    "",
  );
  check(
    (run.phase === "starting") === (world.op === "auto"),
    "lost its own start",
    next,
    "",
  );
  check(
    state.starting === (world.op === "foreign"),
    "lost someone else's start",
    next,
    "",
  );
  check(
    state.holding === world.holding && state.kept === world.kept,
    "lost a fact",
    next,
    "",
  );
  // Normalised so equal futures compare equal.
  return advance(next, 0, timing);
}

const key = ({ state, world, monitor }: Node) =>
  JSON.stringify([state, world, monitor]);

/** From `node`, a core that stays up for the bound ends in idle; answers the final node. */
function staysUp(node: Node, timing: RestartTiming, own: boolean): Node {
  const step = (n: Node, name: string) => {
    const move = moves(n.world, timing).find((m) => m.name === name);
    assert.ok(move, `${name} is possible after ${n.path}`);
    return apply(n, move, timing);
  };
  let n = node;
  if (own) {
    // The scheduler's own core: its next start, which succeeds.
    const due = wakeAt(n.state, timing);
    if (due > 0) n = advance(n, due, timing);
    n = step(n, "time");
    if (n.world.op !== "auto") return n;
    n = step(n, "auto-ok");
  } else {
    if (n.world.holding) n = step(n, "release");
    if (n.world.op === "auto")
      n = step(n, n.world.lost ? "auto-fail" : "auto-ok");
    if (n.world.op === "foreign") n = step(n, "other-ok");
    if (!n.world.core) n = step(step(n, "other-start"), "other-ok");
  }
  n = advance(n, timing.restartBackoff, timing);
  // Its exit alone, with nothing having looked meanwhile, shows it ended the run.
  const exited = step(n, "crash");
  assert.equal(exited.state.run.phase, "idle", `exit after ${n.path}`);
  n = step(n, "time");
  assert.equal(n.state.run.phase, "idle", `after ${n.path}`);
  return n;
}

for (const timing of [
  small,
  { coalesce: 1, restartBackoff: 4, restartLimit: 5 },
  { coalesce: 2, restartBackoff: 3, restartLimit: 1 },
])
  test(`every reachable state keeps the rules (${JSON.stringify(timing)})`, () => {
    const seen = new Map<string, Node>();
    const queue: Node[] = [];
    for (const kept of [true, false])
      for (const holding of [true, false])
        for (const core of [true, false])
          for (const op of ["none", "foreign"] as const)
            for (const reported of [true, false]) {
              if (core && (op !== "none" || reported)) continue;
              if (op === "foreign" && holding) continue;
              const node: Node = {
                state: initial({
                  running: core,
                  reported,
                  kept,
                  holding,
                  starting: op === "foreign",
                }),
                world: { core, op, lost: false, holding, kept, upSince: null },
                monitor: {
                  inRun: false,
                  starts: 0,
                  gaveUp: 0,
                  reports: 0,
                  launch: reported,
                  quiet: false,
                },
                path: `[kept ${kept} holding ${holding} core ${core} ${op} reported ${reported}]`,
                depth: 0,
              };
              seen.set(key(node), node);
              queue.push(node);
            }
    let transitions = 0;
    let depth = 0;
    while (queue.length) {
      const node = queue.shift()!;
      depth = Math.max(depth, node.depth);
      for (const move of moves(node.world, timing)) {
        const next = apply(node, move, timing);
        transitions++;
        if (!seen.has(key(next))) {
          seen.set(key(next), next);
          queue.push(next);
        }
      }
    }
    const phases = new Set([...seen.values()].map((n) => n.state.run.phase));
    assert.deepEqual([...phases].sort(), [
      "gave-up",
      "idle",
      "starting",
      "up",
      "waiting",
    ]);
    assert.ok(
      [...seen.values()].some(
        (n) =>
          n.state.run.phase === "up" &&
          n.state.run.attempts === timing.restartLimit,
      ),
      "the limit's last core runs somewhere",
    );
    for (const node of seen.values()) {
      staysUp(node, timing, false);
      if (wakeAt(node.state, timing) < Infinity) staysUp(node, timing, true);
    }
    assert.ok(
      seen.size > 100,
      `${seen.size} states, ${transitions} transitions, depth ${depth}`,
    );
  });
