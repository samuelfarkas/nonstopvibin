//! The restart policy: tests/golden/restart.json replayed, the named sequences of
//! tests/restart-policy.test.ts, then every state reachable from every starting point with
//! the invariants checked on each transition.

use std::collections::{HashSet, VecDeque};

use nsv_core::restart::{
    Action, Due, Facts, RESTART_TIMING, RestartEvent, RestartState, RestartTiming, Run,
    give_up_text,
};
use serde_json::{Value, json};

// ---- The golden file ----

fn number(value: &Value) -> i64 {
    value.as_i64().unwrap()
}

fn event(value: &Value) -> RestartEvent {
    let flag = |key: &str| value[key].as_bool().unwrap();
    let error = value.get("error").map(|e| e.as_str().unwrap().to_owned());
    match value["type"].as_str().unwrap() {
        "kept" => RestartEvent::Kept(flag("kept")),
        "holding" => RestartEvent::Holding(flag("holding")),
        "starting" => RestartEvent::Starting {
            automatic: flag("automatic"),
        },
        "started" => RestartEvent::Started {
            automatic: flag("automatic"),
            running: flag("running"),
            error,
        },
        "exited" => RestartEvent::Exited { error },
        "time" => RestartEvent::Time,
        other => panic!("unknown event {other}"),
    }
}

fn state_json(state: &RestartState) -> Value {
    let run = match state.run {
        Run::Idle { running, reported } => {
            json!({ "phase": "idle", "running": running, "reported": reported })
        }
        Run::Waiting {
            attempts,
            due,
            reported,
        } => json!({ "phase": "waiting", "attempts": attempts, "due": due, "reported": reported }),
        Run::Starting { attempts, reported } => {
            json!({ "phase": "starting", "attempts": attempts, "reported": reported })
        }
        Run::Up {
            attempts,
            since,
            reported,
        } => json!({ "phase": "up", "attempts": attempts, "since": since, "reported": reported }),
        Run::GaveUp { attempts } => json!({ "phase": "gave-up", "attempts": attempts }),
    };
    json!({ "run": run, "kept": state.kept, "starting": state.starting, "holding": state.holding })
}

/// `±Infinity` are strings in the golden file.
fn wake_json(wake: Option<Due>) -> Value {
    match wake {
        None => json!("Infinity"),
        Some(Due::Now) => json!("-Infinity"),
        Some(Due::At(at)) => json!(at),
    }
}

fn action_json(action: &Action) -> Value {
    match action {
        Action::Start => json!({ "type": "start" }),
        Action::Wake(due) => json!({ "type": "wake", "at": wake_json(Some(*due)) }),
        Action::Report(text) => json!({ "type": "report", "text": text }),
        Action::Show(None) => json!({ "type": "show" }),
        Action::Show(Some(text)) => json!({ "type": "show", "text": text }),
    }
}

#[test]
fn replays_the_golden_restart_sequences() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/restart.json"
    );
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(cases.len(), 137);
    let mut passed = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        if name == "giveUpText" {
            let texts: Vec<Value> = input
                .as_array()
                .unwrap()
                .iter()
                .map(|n| json!(give_up_text(u32::try_from(number(n)).unwrap())))
                .collect();
            assert_eq!(Value::from(texts), case["output"], "{name}");
            passed += 1;
            continue;
        }
        let facts = &input["facts"];
        let flag = |key: &str| facts[key].as_bool().unwrap();
        let timing = RestartTiming {
            coalesce: number(&input["timing"]["coalesce"]),
            restart_backoff: number(&input["timing"]["restartBackoff"]),
            restart_limit: u32::try_from(number(&input["timing"]["restartLimit"])).unwrap(),
        };
        let mut state = RestartState::new(Facts {
            running: flag("running"),
            reported: flag("reported"),
            kept: flag("kept"),
            starting: flag("starting"),
            holding: flag("holding"),
        });
        let mut output = vec![
            json!({ "initial": state_json(&state), "wakeAt": wake_json(state.wake_at(&timing)) }),
        ];
        for step in input["events"].as_array().unwrap() {
            let next = state.transition(event(&step["event"]), number(&step["now"]), &timing);
            state = next.state;
            output.push(json!({
                "state": state_json(&state),
                "actions": next.actions.iter().map(action_json).collect::<Vec<_>>(),
                "wakeAt": wake_json(state.wake_at(&timing)),
            }));
        }
        assert_eq!(Value::from(output), case["output"], "{name}");
        passed += 1;
    }
    assert_eq!(passed, 137);
}

// ---- Named sequences ----

const SMALL: RestartTiming = RestartTiming {
    coalesce: 1,
    restart_backoff: 4,
    restart_limit: 3,
};

fn fresh(facts: Facts) -> RestartState {
    RestartState::new(Facts {
        kept: true,
        ..facts
    })
}

const TIME: RestartEvent = RestartEvent::Time;

fn auto_ok() -> RestartEvent {
    RestartEvent::Started {
        automatic: true,
        running: true,
        error: None,
    }
}

fn auto_failed() -> RestartEvent {
    RestartEvent::Started {
        automatic: true,
        running: false,
        error: Some("failed".to_owned()),
    }
}

fn crash() -> RestartEvent {
    RestartEvent::Exited {
        error: Some("crashed".to_owned()),
    }
}

/// Feeds `(event, now)` pairs in order; answers the last state and every action, in order.
fn drive(
    mut state: RestartState,
    events: Vec<(RestartEvent, i64)>,
    timing: &RestartTiming,
) -> (RestartState, Vec<Action>) {
    let mut actions = Vec::new();
    for (event, now) in events {
        let step = state.transition(event, now, timing);
        state = step.state;
        actions.extend(step.actions);
    }
    (state, actions)
}

fn reports(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Report(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Up to the limit's last automatic start, whose core is now running since 6.
fn last_attempt_up() -> (RestartState, i64) {
    // Starts at 0, 2 and 5: each failed start waits 1, then 2 before the next.
    let (state, _) = drive(
        fresh(Facts::default()),
        vec![
            (TIME, 0),
            (auto_failed(), 1),
            (TIME, 2),
            (auto_failed(), 3),
            (TIME, 5),
            (auto_ok(), 6),
        ],
        &SMALL,
    );
    assert_eq!(
        state.run,
        Run::Up {
            attempts: 3,
            since: 6,
            reported: true
        }
    );
    (state, 6)
}

#[test]
fn the_limits_last_core_up_past_the_bound_with_nothing_looking_exits_into_a_fresh_run() {
    let (state, since) = last_attempt_up();
    let bound = SMALL.restart_backoff;
    let (after, actions) = drive(
        state,
        vec![(crash(), since + bound), (TIME, since + bound)],
        &SMALL,
    );
    assert_eq!(reports(&actions), Vec::<String>::new());
    assert_eq!(
        after.run,
        Run::Starting {
            attempts: 1,
            reported: false
        }
    );
    // Just under the bound it was one more failure, and the limit is reached.
    let (before, actions) = drive(
        state,
        vec![(crash(), since + bound - 1), (TIME, since + bound - 1)],
        &SMALL,
    );
    assert!(matches!(before.run, Run::GaveUp { .. }));
    assert_eq!(reports(&actions), [give_up_text(3)]);
}

#[test]
fn someone_elses_start_that_leaves_a_core_running_ends_the_run() {
    let (state, since) = last_attempt_up();
    let (next, actions) = drive(
        state,
        vec![
            (crash(), since + 1),
            (RestartEvent::Starting { automatic: false }, since + 1),
            (
                RestartEvent::Started {
                    automatic: false,
                    running: true,
                    error: None,
                },
                since + 2,
            ),
            (crash(), since + 3),
            (TIME, since + 3),
        ],
        &SMALL,
    );
    assert_eq!(
        next.run,
        Run::Starting {
            attempts: 1,
            reported: false
        }
    );
    assert!(!reports(&actions).contains(&give_up_text(3)));
    let (alone, _) = drive(state, vec![(crash(), since + 1), (TIME, since + 1)], &SMALL);
    assert!(matches!(alone.run, Run::GaveUp { .. }));
}

#[test]
fn a_start_in_flight_is_decided_by_its_own_end() {
    let (state, _) = drive(fresh(Facts::default()), vec![(TIME, 0)], &SMALL);
    assert!(matches!(state.run, Run::Starting { .. }));
    let (during, actions) = drive(state, vec![(TIME, 1), (crash(), 2), (TIME, 100)], &SMALL);
    assert_eq!(during, state);
    assert_eq!(actions, []);
    // The backoff counts from the start's end, and the run keeps its count.
    let (failed, _) = drive(during, vec![(auto_failed(), 150)], &SMALL);
    assert_eq!(
        failed.run,
        Run::Waiting {
            attempts: 1,
            due: 151,
            reported: true
        }
    );
}

#[test]
fn only_a_failure_the_launch_reported_counts_as_this_runs_entry() {
    let (_, launch) = drive(
        fresh(Facts {
            reported: true,
            ..Facts::default()
        }),
        vec![(TIME, 0), (auto_failed(), 1)],
        &SMALL,
    );
    assert_eq!(reports(&launch), Vec::<String>::new());
    // Started by the launch, then crashed: nothing reported it yet.
    let (_, crashed) = drive(
        fresh(Facts {
            running: true,
            ..Facts::default()
        }),
        vec![
            (crash(), 0),
            (TIME, 0),
            (auto_failed(), 1),
            (TIME, 2),
            (auto_failed(), 3),
        ],
        &SMALL,
    );
    assert_eq!(reports(&crashed), ["failed"]);
}

#[test]
fn a_blocked_profile_asks_for_no_wakeup_and_asks_again_once_the_obstacle_ends() {
    let cases = [
        (RestartEvent::Holding(true), RestartEvent::Holding(false)),
        (
            RestartEvent::Starting { automatic: false },
            RestartEvent::Started {
                automatic: false,
                running: false,
                error: None,
            },
        ),
        (RestartEvent::Kept(false), RestartEvent::Kept(true)),
    ];
    for (blocked, ended) in cases {
        let (state, actions) = drive(fresh(Facts::default()), vec![(blocked.clone(), 0)], &SMALL);
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Wake(_))),
            "{blocked:?}"
        );
        assert_eq!(state.wake_at(&SMALL), None);
        let (_, actions) = drive(state, vec![(ended, 5)], &SMALL);
        assert_eq!(actions, [Action::Wake(Due::Now)]);
    }
}

#[test]
fn a_failed_start_by_someone_else_changes_nothing() {
    let someone_fails = || {
        vec![
            (RestartEvent::Starting { automatic: false }, 10),
            (
                RestartEvent::Started {
                    automatic: false,
                    running: false,
                    error: Some("no".to_owned()),
                },
                11,
            ),
        ]
    };
    let (mid_run, _) = drive(
        fresh(Facts::default()),
        vec![(TIME, 0), (auto_failed(), 1), (TIME, 2), (auto_failed(), 3)],
        &SMALL,
    );
    assert_eq!(drive(mid_run, someone_fails(), &SMALL).0, mid_run);
    let (gave_up, _) = drive(
        mid_run,
        vec![(TIME, 5), (auto_failed(), 6), (TIME, 6)],
        &SMALL,
    );
    assert!(matches!(gave_up.run, Run::GaveUp { .. }));
    let (after, actions) = drive(gave_up, someone_fails(), &SMALL);
    assert_eq!(after, gave_up);
    assert_eq!(actions, []);
}

#[test]
fn with_the_default_timing_waits_double_from_2_s_to_256_s_then_300_s_and_the_tenth_failure_gives_up()
 {
    let mut state = fresh(Facts::default());
    let mut now = 0;
    let mut waits = Vec::new();
    let mut all = Vec::new();
    loop {
        let step = state.transition(TIME, now, &RESTART_TIMING);
        state = step.state;
        all.extend(step.actions);
        if matches!(state.run, Run::GaveUp { .. }) {
            break;
        }
        assert!(matches!(state.run, Run::Starting { .. }));
        let failed = state.transition(auto_failed(), now + 1, &RESTART_TIMING);
        state = failed.state;
        all.extend(failed.actions);
        if let Some(Due::At(due)) = state.wake_at(&RESTART_TIMING) {
            waits.push(due - (now + 1));
            now = due;
        }
    }
    assert_eq!(
        waits,
        [2, 4, 8, 16, 32, 64, 128, 256, 300].map(|s| s * 1000)
    );
    assert_eq!(all.iter().filter(|a| **a == Action::Start).count(), 10);
    assert_eq!(reports(&all), ["failed".to_owned(), give_up_text(10)]);
    assert_eq!(all.last(), Some(&Action::Show(Some(give_up_text(10)))));
}

// ---- Every reachable state ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Op {
    None,
    Auto,
    Foreign,
}

/// What really happens to the profile, which the policy only hears about through events.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the World of tests/restart-policy.test.ts"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct World {
    /// A core runs and no start is in flight.
    core: bool,
    op: Op,
    /// The scheduler's core exited before its start ended.
    lost: bool,
    holding: bool,
    kept: bool,
    /// When the start of the running core ended, if the scheduler made it.
    up_since: Option<i64>,
}

/// The rules, kept apart from the policy: one run of failures as the specification defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Monitor {
    in_run: bool,
    starts: u32,
    gave_up: u32,
    reports: u32,
    /// The launch reported this profile's failed start, and nothing ran since.
    launch: bool,
    /// This run's first failure was the launch's.
    quiet: bool,
}

#[derive(Debug, Clone)]
struct Node {
    state: RestartState,
    world: World,
    monitor: Monitor,
    /// The moves that first reached it.
    path: String,
    depth: usize,
}

struct Move {
    name: &'static str,
    dt: Option<i64>,
    event: Option<RestartEvent>,
    world: World,
}

/// The moves possible from `world`; every event happens at now = 0, after an advance.
#[expect(
    clippy::too_many_lines,
    reason = "one entry per move, as the TypeScript test"
)]
fn moves(world: World, timing: &RestartTiming) -> Vec<Move> {
    let event = |name, event, next: World| Move {
        name,
        dt: None,
        event: Some(event),
        world: next,
    };
    let mut list = vec![
        Move {
            name: "advance",
            dt: Some(1),
            event: None,
            world,
        },
        Move {
            name: "advance-bound",
            dt: Some(timing.restart_backoff + 1),
            event: None,
            world,
        },
        event("time", TIME, world),
        event(
            "kept",
            RestartEvent::Kept(!world.kept),
            World {
                kept: !world.kept,
                ..world
            },
        ),
    ];
    if world.holding {
        list.push(event(
            "release",
            RestartEvent::Holding(false),
            World {
                holding: false,
                ..world
            },
        ));
        if world.core {
            list.push(event(
                "stopped",
                RestartEvent::Exited { error: None },
                World {
                    core: false,
                    up_since: None,
                    ..world
                },
            ));
        }
    } else {
        list.push(event(
            "hold",
            RestartEvent::Holding(true),
            World {
                holding: true,
                ..world
            },
        ));
    }
    if world.core {
        list.push(event(
            "crash",
            crash(),
            World {
                core: false,
                up_since: None,
                ..world
            },
        ));
    }
    if world.op == Op::Auto {
        if !world.lost {
            list.push(event(
                "auto-lost",
                crash(),
                World {
                    lost: true,
                    ..world
                },
            ));
        }
        list.push(event(
            "auto-ok",
            RestartEvent::Started {
                automatic: true,
                running: !world.lost,
                error: world.lost.then(|| "crashed".to_owned()),
            },
            World {
                op: Op::None,
                lost: false,
                core: !world.lost,
                up_since: (!world.lost).then_some(0),
                ..world
            },
        ));
        list.push(event(
            "auto-fail",
            auto_failed(),
            World {
                op: Op::None,
                lost: false,
                ..world
            },
        ));
    }
    if world.op == Op::None && !world.core && !world.holding {
        list.push(event(
            "other-start",
            RestartEvent::Starting { automatic: false },
            World {
                op: Op::Foreign,
                ..world
            },
        ));
    }
    if world.op == Op::Foreign {
        list.push(event(
            "other-ok",
            RestartEvent::Started {
                automatic: false,
                running: true,
                error: None,
            },
            World {
                op: Op::None,
                core: true,
                up_since: None,
                ..world
            },
        ));
        list.push(event(
            "other-fail",
            RestartEvent::Started {
                automatic: false,
                running: false,
                error: Some("no".to_owned()),
            },
            World {
                op: Op::None,
                ..world
            },
        ));
    }
    list
}

/// Moves the clock forward by `dt`: times are kept relative to now, and those long past are all alike.
fn advance(node: &Node, dt: i64, timing: &RestartTiming) -> Node {
    let bound = timing.restart_backoff;
    let mut next = node.clone();
    next.state.run = match node.state.run {
        Run::Waiting {
            attempts,
            due,
            reported,
        } => Run::Waiting {
            attempts,
            due: (due - dt).max(0),
            reported,
        },
        Run::Up {
            attempts,
            since,
            reported,
        } => Run::Up {
            attempts,
            since: (since - dt).max(-bound),
            reported,
        },
        run => run,
    };
    next.world.up_since = node.world.up_since.map(|since| (since - dt).max(-bound));
    next
}

fn check(condition: bool, message: &str, node: &Node) {
    assert!(condition, "{message} after {}", node.path);
}

/// One move, the policy's answer to it, and every invariant on the way.
#[expect(
    clippy::too_many_lines,
    reason = "one check per invariant, as the TypeScript test"
)]
fn apply(node: &Node, step: &Move, timing: &RestartTiming) -> Node {
    if let Some(dt) = step.dt {
        let mut next = advance(node, dt, timing);
        next.path = format!("{} {}", node.path, step.name);
        next.depth = node.depth + 1;
        return next;
    }
    let before = node.state;
    let mut monitor = node.monitor;
    let end_run = |monitor: &mut Monitor| {
        monitor.in_run = false;
        monitor.starts = 0;
        monitor.gave_up = 0;
        monitor.reports = 0;
        monitor.launch = false;
    };
    // The scheduler's core has been up for the bound: that run is over, whether or not anyone looked.
    if node.world.core
        && node
            .world
            .up_since
            .is_some_and(|since| -since >= timing.restart_backoff)
    {
        end_run(&mut monitor);
    }
    if step.name == "other-ok" {
        end_run(&mut monitor);
    }
    let mut world = step.world;
    let event = step.event.clone().expect("an event");
    let answer = before.transition(event, 0, timing);
    let state = answer.state;
    let mut actions = answer.actions;
    let label = Node {
        state,
        world,
        monitor,
        path: format!("{} {}", node.path, step.name),
        depth: node.depth + 1,
    };
    if actions.contains(&Action::Start) {
        check(
            !node.world.core
                && node.world.op == Op::None
                && !node.world.holding
                && node.world.kept
                && step.name == "time"
                && !matches!(before.run, Run::GaveUp { .. }),
            "started a profile that runs, is busy, held, without a subscription or given up",
            &label,
        );
        if !monitor.in_run {
            monitor.in_run = true;
            monitor.quiet = monitor.launch;
        }
        monitor.starts += 1;
        world.op = Op::Auto;
        // The pool says so from inside the start call.
        let began = state.transition(RestartEvent::Starting { automatic: true }, 0, timing);
        check(
            began.state == state,
            "its own start's notice changed the state",
            &label,
        );
        actions.extend(
            began
                .actions
                .into_iter()
                .filter(|a| !matches!(a, Action::Wake(_))),
        );
    }
    let all = reports(&actions);
    let give_ups = u32::try_from(
        all.iter()
            .filter(|text| text.starts_with("Stopped"))
            .count(),
    )
    .unwrap();
    monitor.gave_up += give_ups;
    monitor.reports += u32::try_from(all.len()).unwrap() - give_ups;
    let next = Node {
        state,
        world,
        monitor,
        path: label.path,
        depth: label.depth,
    };
    let run = state.run;
    let gave_up = |run: Run| matches!(run, Run::GaveUp { .. });
    check(
        monitor.starts <= timing.restart_limit,
        "more starts than the limit in one run",
        &next,
    );
    check(
        !matches!(run, Run::Idle { .. }) == monitor.in_run,
        "the run ended at the wrong moment",
        &next,
    );
    check(monitor.gave_up <= 1, "gave up twice in one run", &next);
    check(
        give_ups == u32::from(!gave_up(before.run) && gave_up(run)),
        "the give-up line came without giving up, or giving up without it",
        &next,
    );
    if gave_up(before.run) && !gave_up(run) {
        check(
            step.name == "other-ok",
            "left the give-up without someone else's start",
            &next,
        );
    }
    check(
        monitor.reports <= u32::from(!monitor.quiet),
        "reported a run twice",
        &next,
    );
    let wake = state.wake_at(timing);
    let wakes: Vec<&Action> = actions
        .iter()
        .filter(|a| matches!(a, Action::Wake(_)))
        .collect();
    check(
        wakes.len() == usize::from(wake.is_some())
            && wake.is_none_or(|due| wakes.first() == Some(&&Action::Wake(due))),
        "the wakeup asked for is not the state's",
        &next,
    );
    if step.name == "time" {
        check(
            !matches!(wake, Some(Due::Now | Due::At(i64::MIN..=0))),
            "due again at once without a change: a spin",
            &next,
        );
    }
    let free = world.kept && world.op == Op::None && !world.holding && !world.core;
    check(
        wake.is_some() == (free && !gave_up(run)),
        "a wakeup while it cannot start, or none while it should",
        &next,
    );
    // What the policy believes is what happened.
    check(
        matches!(run, Run::Up { .. } | Run::Idle { running: true, .. }) == world.core,
        "believes the wrong thing about its core",
        &next,
    );
    check(
        matches!(run, Run::Starting { .. }) == (world.op == Op::Auto),
        "lost its own start",
        &next,
    );
    check(
        state.starting == (world.op == Op::Foreign),
        "lost someone else's start",
        &next,
    );
    check(
        state.holding == world.holding && state.kept == world.kept,
        "lost a fact",
        &next,
    );
    // Normalised so equal futures compare equal.
    advance(&next, 0, timing)
}

fn key(node: &Node) -> (RestartState, World, Monitor) {
    (node.state, node.world, node.monitor)
}

fn step(node: &Node, name: &str, timing: &RestartTiming) -> Node {
    let found = moves(node.world, timing)
        .into_iter()
        .find(|m| m.name == name);
    let found = found.unwrap_or_else(|| panic!("{name} is possible after {}", node.path));
    apply(node, &found, timing)
}

/// From `node`, a core that stays up for the bound ends in idle; answers the final node.
fn stays_up(node: &Node, timing: &RestartTiming, own: bool) -> Node {
    let mut n = node.clone();
    if own {
        // The scheduler's own core: its next start, which succeeds.
        if let Some(Due::At(due)) = n.state.wake_at(timing)
            && due > 0
        {
            n = advance(&n, due, timing);
        }
        n = step(&n, "time", timing);
        if n.world.op != Op::Auto {
            return n;
        }
        n = step(&n, "auto-ok", timing);
    } else {
        if n.world.holding {
            n = step(&n, "release", timing);
        }
        if n.world.op == Op::Auto {
            n = step(
                &n,
                if n.world.lost { "auto-fail" } else { "auto-ok" },
                timing,
            );
        }
        if n.world.op == Op::Foreign {
            n = step(&n, "other-ok", timing);
        }
        if !n.world.core {
            n = step(&step(&n, "other-start", timing), "other-ok", timing);
        }
    }
    n = advance(&n, timing.restart_backoff, timing);
    // Its exit alone, with nothing having looked meanwhile, shows it ended the run.
    let exited = step(&n, "crash", timing);
    assert!(
        matches!(exited.state.run, Run::Idle { .. }),
        "exit after {}",
        n.path
    );
    n = step(&n, "time", timing);
    assert!(matches!(n.state.run, Run::Idle { .. }), "after {}", n.path);
    n
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the exploration of tests/restart-policy.test.ts"
)]
fn every_reachable_state_keeps_the_rules() {
    for timing in [
        SMALL,
        RestartTiming {
            coalesce: 1,
            restart_backoff: 4,
            restart_limit: 5,
        },
        RestartTiming {
            coalesce: 2,
            restart_backoff: 3,
            restart_limit: 1,
        },
    ] {
        let mut seen: HashSet<(RestartState, World, Monitor)> = HashSet::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut queue = VecDeque::new();
        for kept in [true, false] {
            for holding in [true, false] {
                for core in [true, false] {
                    for op in [Op::None, Op::Foreign] {
                        for reported in [true, false] {
                            if (core && (op != Op::None || reported))
                                || (op == Op::Foreign && holding)
                            {
                                continue;
                            }
                            let node = Node {
                                state: RestartState::new(Facts {
                                    running: core,
                                    reported,
                                    kept,
                                    holding,
                                    starting: op == Op::Foreign,
                                }),
                                world: World {
                                    core,
                                    op,
                                    lost: false,
                                    holding,
                                    kept,
                                    up_since: None,
                                },
                                monitor: Monitor {
                                    in_run: false,
                                    starts: 0,
                                    gave_up: 0,
                                    reports: 0,
                                    launch: reported,
                                    quiet: false,
                                },
                                path: format!(
                                    "[kept {kept} holding {holding} core {core} {op:?} reported {reported}]"
                                ),
                                depth: 0,
                            };
                            seen.insert(key(&node));
                            nodes.push(node.clone());
                            queue.push_back(node);
                        }
                    }
                }
            }
        }
        let mut transitions = 0;
        while let Some(node) = queue.pop_front() {
            for found in moves(node.world, &timing) {
                let next = apply(&node, &found, &timing);
                transitions += 1;
                if seen.insert(key(&next)) {
                    nodes.push(next.clone());
                    queue.push_back(next);
                }
            }
        }
        let phases: HashSet<&str> = nodes
            .iter()
            .map(|n| match n.state.run {
                Run::Idle { .. } => "idle",
                Run::Waiting { .. } => "waiting",
                Run::Starting { .. } => "starting",
                Run::Up { .. } => "up",
                Run::GaveUp { .. } => "gave-up",
            })
            .collect();
        assert_eq!(phases.len(), 5, "{phases:?}");
        assert!(
            nodes.iter().any(|n| matches!(n.state.run, Run::Up { attempts, .. } if attempts == timing.restart_limit)),
            "the limit's last core runs somewhere"
        );
        for node in &nodes {
            stays_up(node, &timing, false);
            if node.state.wake_at(&timing).is_some() {
                stays_up(node, &timing, true);
            }
        }
        assert!(
            seen.len() > 100,
            "{} states, {transitions} transitions",
            seen.len()
        );
    }
}
