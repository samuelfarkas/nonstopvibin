//! `updateChecker` in desktop/updates.ts as a state machine: checks, downloads and dialogs run
//! one at a time, a check while one is busy (a manual check during a download included) is
//! dropped, and once an update is ready only a manual check shows it again.

use nsv_domain::helper::DialogKind;

/// What the shell must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    /// Look for an update, then download and verify it; [`Machine::checked`] takes the result.
    Check,
    Show(DialogKind),
}

/// How a check ended. `T` is the verified update, ready to install.
#[derive(Debug)]
pub(crate) enum Checked<T> {
    Ready(T),
    UpToDate,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    Checking { manual: bool },
    Dialog(DialogKind),
}

#[derive(Debug)]
pub(crate) struct Machine<T> {
    busy: Busy,
    ready: Option<T>,
}

impl<T> Machine<T> {
    pub(crate) const fn new() -> Self {
        Self {
            busy: Busy::Idle,
            ready: None,
        }
    }

    /// The timer fired (`manual` false), or the user chose "Check for updates…".
    pub(crate) fn check(&mut self, manual: bool) -> Option<Command> {
        if self.busy != Busy::Idle {
            return None;
        }
        if self.ready.is_some() {
            return manual.then(|| self.show(DialogKind::UpdateReady));
        }
        self.busy = Busy::Checking { manual };
        Some(Command::Check)
    }

    pub(crate) fn checked(&mut self, result: Checked<T>) -> Option<Command> {
        let Busy::Checking { manual } = self.busy else {
            return None;
        };
        self.busy = Busy::Idle;
        match result {
            Checked::Ready(update) => {
                self.ready = Some(update);
                Some(self.show(DialogKind::UpdateReady))
            }
            Checked::UpToDate => manual.then(|| self.show(DialogKind::NoUpdates)),
            Checked::Failed => manual.then(|| self.show(DialogKind::UpdateFailed)),
        }
    }

    /// "Restart to update" in the update-ready dialog: the update to install.
    pub(crate) fn restart(&mut self) -> Option<T> {
        if self.busy == Busy::Dialog(DialogKind::UpdateReady) {
            self.ready.take()
        } else {
            None
        }
    }

    /// A dialog closed, or the helper exited with it (`None`: every dialog).
    pub(crate) fn closed(&mut self, kind: Option<DialogKind>) {
        if let Busy::Dialog(open) = self.busy
            && kind.is_none_or(|kind| kind == open)
        {
            self.busy = Busy::Idle;
        }
    }

    fn show(&mut self, kind: DialogKind) -> Command {
        self.busy = Busy::Dialog(kind);
        Command::Show(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY: Option<Command> = Some(Command::Show(DialogKind::UpdateReady));

    #[test]
    fn checks_serialize_and_a_found_update_asks_once_then_only_on_request() {
        let mut machine = Machine::new();
        assert_eq!(machine.check(false), Some(Command::Check));
        // A timer tick and a manual check during the download are dropped.
        assert_eq!(machine.check(false), None);
        assert_eq!(machine.check(true), None);
        assert_eq!(machine.checked(Checked::Ready("0.3.0")), READY);
        // While the dialog shows, nothing else starts.
        assert_eq!(machine.check(true), None);
        machine.closed(Some(DialogKind::UpdateReady));
        assert_eq!(machine.restart(), None, "Later was chosen");
        // The timer no longer checks or asks; a manual check asks again.
        assert_eq!(machine.check(false), None);
        assert_eq!(machine.check(true), READY);
        assert_eq!(machine.restart(), Some("0.3.0"));
        assert_eq!(machine.restart(), None);
    }

    #[test]
    fn only_manual_checks_report_no_update_or_a_failure_and_failures_retry() {
        let mut machine = Machine::<()>::new();
        assert_eq!(machine.check(false), Some(Command::Check));
        assert_eq!(machine.checked(Checked::UpToDate), None);
        assert_eq!(machine.check(false), Some(Command::Check));
        assert_eq!(machine.checked(Checked::Failed), None);
        assert_eq!(machine.check(true), Some(Command::Check));
        assert_eq!(
            machine.checked(Checked::UpToDate),
            Some(Command::Show(DialogKind::NoUpdates))
        );
        machine.closed(Some(DialogKind::NoUpdates));
        assert_eq!(machine.check(true), Some(Command::Check));
        assert_eq!(
            machine.checked(Checked::Failed),
            Some(Command::Show(DialogKind::UpdateFailed))
        );
        assert_eq!(machine.check(false), None);
        // The helper exited with the dialog: checks resume.
        machine.closed(None);
        assert_eq!(machine.check(false), Some(Command::Check));
    }

    #[test]
    fn another_dialog_closing_keeps_the_ready_dialog_and_stray_results_are_ignored() {
        let mut machine = Machine::new();
        assert_eq!(
            machine.checked(Checked::Ready(1)),
            None,
            "no check is running"
        );
        machine.check(true);
        machine.checked(Checked::Ready(2));
        machine.closed(Some(DialogKind::NoUpdates));
        assert_eq!(machine.check(true), None);
        assert_eq!(machine.restart(), Some(2));
    }
}
