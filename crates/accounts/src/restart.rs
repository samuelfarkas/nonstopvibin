//! When the scheduler starts a profile the app keeps running (it has a subscription): one
//! profile's state, the events that change it and what to do next. Pure: no timers, pool or
//! store, and time is an argument in milliseconds, so a decision depends only on what
//! happened, in order, and when.

use nsv_core::pool::CoreEvent;

/// The restart periods of the scheduler's timing, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartTiming {
    /// The wait after the first failed start; each further failure doubles it. Also the
    /// shortest wait between the scheduler's wakeups.
    pub coalesce: i64,
    /// Longest wait between automatic starts, and how long a core must stay up to end a run
    /// of failures.
    pub restart_backoff: i64,
    /// Automatic starts in one run of failures before the scheduler stops starting the
    /// profile.
    pub restart_limit: u32,
}

/// The scheduler's timing: waits double from 2 s to 300 s, and the tenth failure gives up.
pub const RESTART_TIMING: RestartTiming = RestartTiming {
    coalesce: 2000,
    restart_backoff: 300_000,
    restart_limit: 10,
};

/// Where the profile is in a run of failures: automatic starts in a row that failed, or whose
/// core exited before it had been up for `restart_backoff` since its start ended. `reported`:
/// the run already has its error-list entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Run {
    /// No run. `running`: its core runs. `reported`: the launch restore reported its failed
    /// start.
    Idle { running: bool, reported: bool },
    /// Its core is down after `attempts` automatic starts; the next may begin at `due`.
    Waiting {
        attempts: u32,
        due: i64,
        reported: bool,
    },
    /// Automatic start number `attempts` is in flight.
    Starting { attempts: u32, reported: bool },
    /// That start ended at `since` with its core running.
    Up {
        attempts: u32,
        since: i64,
        reported: bool,
    },
    /// The scheduler stopped starting it after `attempts` automatic starts.
    GaveUp { attempts: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartState {
    pub run: Run,
    /// It has a subscription and exists.
    pub kept: bool,
    /// A start by someone else is in flight.
    pub starting: bool,
    /// A stop or a credential swap is in flight.
    pub holding: bool,
}

/// What can happen to a profile, told as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartEvent {
    /// It gained or lost its last subscription, or no longer exists.
    Kept(bool),
    /// A stop or credential swap began, or none is in flight any more.
    Holding(bool),
    /// A start began; `automatic`: the scheduler's, which this policy began itself.
    Starting { automatic: bool },
    /// That start ended; `running`: with its core running; `error`: why not.
    Started {
        automatic: bool,
        running: bool,
        error: Option<String>,
    },
    /// Its core went away; `error` unless it was stopped on purpose.
    Exited { error: Option<String> },
    /// A wakeup this policy asked for came due.
    Time,
}

impl From<CoreEvent> for RestartEvent {
    fn from(event: CoreEvent) -> Self {
        match event {
            CoreEvent::Starting { automatic } => Self::Starting { automatic },
            CoreEvent::Started {
                automatic,
                running,
                error,
            } => Self::Started {
                automatic,
                running,
                error,
            },
            CoreEvent::Exited { error } => Self::Exited { error },
            CoreEvent::Holding(holding) => Self::Holding(holding),
        }
    }
}

/// When the profile next wants [`RestartEvent::Time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Due {
    /// At once.
    Now,
    At(i64),
}

impl Due {
    /// Whether it is due by `now`.
    #[must_use]
    pub const fn by(self, now: i64) -> bool {
        match self {
            Self::Now => true,
            Self::At(at) => at <= now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Start,
    /// Send [`RestartEvent::Time`] then. A step without one wants no wakeup.
    Wake(Due),
    /// One error-list entry.
    Report(String),
    /// What the profile shows instead of its last failure, or nothing.
    Show(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub state: RestartState,
    pub actions: Vec<Action>,
}

/// What the scheduler knows about a profile when it starts watching it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts of initial() in restart-policy.ts"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facts {
    pub running: bool,
    /// The launch restore reported its failed start.
    pub reported: bool,
    pub kept: bool,
    pub starting: bool,
    pub holding: bool,
}

impl RestartState {
    #[must_use]
    pub const fn new(facts: Facts) -> Self {
        Self {
            run: Run::Idle {
                running: facts.running,
                reported: facts.reported && !facts.running,
            },
            kept: facts.kept,
            starting: facts.starting,
            holding: facts.holding,
        }
    }

    /// When the profile wants [`RestartEvent::Time`]; `None` while it cannot or need not be
    /// started.
    #[must_use]
    pub const fn wake_at(&self, timing: &RestartTiming) -> Option<Due> {
        if !self.kept || self.starting || self.holding {
            return None;
        }
        match self.run {
            Run::Idle { running, .. } => {
                if running {
                    None
                } else {
                    Some(Due::Now)
                }
            }
            // At the limit the next step is giving up, which waits for nothing.
            Run::Waiting { attempts, due, .. } => Some(if attempts >= timing.restart_limit {
                Due::Now
            } else {
                Due::At(due)
            }),
            Run::Starting { .. } | Run::Up { .. } | Run::GaveUp { .. } => None,
        }
    }

    /// The state after `event` at `now`, and what to do about it.
    #[must_use]
    pub fn transition(self, event: RestartEvent, now: i64, timing: &RestartTiming) -> Step {
        let mut actions = Vec::new();
        let mut next = self;
        // A core up for the whole bound ended the run, whether or not anything looked meanwhile.
        if let Run::Up { since, .. } = next.run
            && now.saturating_sub(since) >= timing.restart_backoff
        {
            next.run = Run::Idle {
                running: true,
                reported: false,
            };
        }
        match event {
            RestartEvent::Kept(kept) => next.kept = kept,
            RestartEvent::Holding(holding) => next.holding = holding,
            RestartEvent::Starting { automatic } => next.starting |= !automatic,
            RestartEvent::Started {
                automatic: false,
                running,
                ..
            } => {
                next.starting = false;
                // Someone else's start left a core running: the run ends, given up or not.
                if running {
                    if matches!(next.run, Run::GaveUp { .. }) {
                        actions.push(Action::Show(None));
                    }
                    next.run = Run::Idle {
                        running: true,
                        reported: false,
                    };
                }
            }
            RestartEvent::Started {
                automatic: true,
                running,
                error,
            } => {
                if let Run::Starting { attempts, reported } = next.run {
                    next.run = if running {
                        Run::Up {
                            attempts,
                            since: now,
                            reported,
                        }
                    } else {
                        failed(attempts, now, reported, error, timing, &mut actions)
                    };
                }
            }
            RestartEvent::Exited { error } => match next.run {
                // While a start is in flight its own end says whether a core runs.
                Run::Idle {
                    running: true,
                    reported,
                } => {
                    next.run = Run::Idle {
                        running: false,
                        reported,
                    };
                }
                Run::Up {
                    attempts,
                    since,
                    reported,
                } => next.run = failed(attempts, since, reported, error, timing, &mut actions),
                _ => {}
            },
            RestartEvent::Time => next.run = next.on_time(now, timing, &mut actions),
        }
        if let Some(due) = next.wake_at(timing) {
            actions.push(Action::Wake(due));
        }
        Step {
            state: next,
            actions,
        }
    }
}

impl RestartState {
    /// The run after a wakeup at `now`: a start or the give-up when one is due.
    fn on_time(&self, now: i64, timing: &RestartTiming, actions: &mut Vec<Action>) -> Run {
        if !self.wake_at(timing).is_some_and(|due| due.by(now)) {
            return self.run;
        }
        match self.run {
            Run::Waiting { attempts, .. } if attempts >= timing.restart_limit => {
                let text = give_up_text(attempts);
                actions.push(Action::Report(text.clone()));
                actions.push(Action::Show(Some(text)));
                Run::GaveUp { attempts }
            }
            // Due while idle means no core runs.
            Run::Idle { reported, .. } => {
                actions.push(Action::Start);
                Run::Starting {
                    attempts: 1,
                    reported,
                }
            }
            Run::Waiting {
                attempts, reported, ..
            } => {
                actions.push(Action::Start);
                Run::Starting {
                    attempts: attempts.saturating_add(1),
                    reported,
                }
            }
            run => run,
        }
    }
}

/// Automatic start `attempts`, which ended at `ended`, failed or lost its core too soon.
fn failed(
    attempts: u32,
    ended: i64,
    reported: bool,
    error: Option<String>,
    timing: &RestartTiming,
    actions: &mut Vec<Action>,
) -> Run {
    let has_error = error.is_some();
    if let Some(text) = error
        && !reported
    {
        actions.push(Action::Report(text));
    }
    let factor = 2_i64
        .checked_pow(attempts.saturating_sub(1))
        .unwrap_or(i64::MAX);
    Run::Waiting {
        attempts,
        due: ended.saturating_add(
            timing
                .coalesce
                .saturating_mul(factor)
                .min(timing.restart_backoff),
        ),
        reported: reported || has_error,
    }
}

#[must_use]
pub fn give_up_text(attempts: u32) -> String {
    format!("Stopped restarting after {attempts} failed starts in a row. Use Restart to try again.")
}
