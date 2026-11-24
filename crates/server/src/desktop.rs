//! The desktop app's part of the daemon: the tray's status and the events for its shell. A
//! headless daemon has none.

use std::sync::{Arc, Mutex, Weak};

use nsv_accounts::cores::Cores;
use nsv_desktop::{HostEvent, TrayStatus};
use nsv_domain::sync::locked;
use nsv_store::ProfileFacts;
use tokio::sync::{broadcast, watch};

/// What the desktop's shell follows. Clones share it.
#[derive(Debug, Clone)]
pub struct Desktop {
    status: watch::Sender<TrayStatus>,
    events: broadcast::Sender<HostEvent>,
    /// Subscribed at creation for the shell, which starts once the daemon restored: it still
    /// hears a second launch from meanwhile.
    first: Arc<Mutex<Option<broadcast::Receiver<HostEvent>>>>,
}

impl Default for Desktop {
    fn default() -> Self {
        let (events, first) = broadcast::channel(8);
        Self {
            status: watch::Sender::new(TrayStatus::default()),
            events,
            first: Arc::new(Mutex::new(Some(first))),
        }
    }
}

impl Desktop {
    /// Serving profiles by name, and whether any request streams.
    #[must_use]
    pub fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.status.subscribe()
    }

    /// Sign-ins and second launches: the first receiver from this desktop's creation on, later
    /// ones from their call on.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<HostEvent> {
        locked(&self.first)
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }

    pub(crate) fn sign_in_accepted(&self) {
        // No receiver means no shell: nothing to bring forward.
        let _ = self.events.send(HostEvent::SignInAccepted);
    }

    pub(crate) fn show_main(&self) {
        let _ = self.events.send(HostEvent::ShowMain);
    }

    /// `Application.status()`: a dormant profile still serves, since its core starts on the next
    /// request. Tells the tray only when something it shows changed.
    fn update(&self, cores: &Cores, facts: &watch::Receiver<ProfileFacts>) {
        let facts = Arc::clone(&facts.borrow());
        let status = cores.statuses(|status| {
            let mut tray = TrayStatus::default();
            for fact in facts.iter() {
                let status = status(&fact.id);
                if status.serving {
                    tray.running.push(fact.name.clone());
                    tray.streaming |= status.streaming;
                }
            }
            tray
        });
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }

    /// Follows each change the pool, the scheduler or the store's profile facts publish, while
    /// `cores` lives.
    pub(crate) fn follow(&self, cores: &Arc<Cores>, mut facts: watch::Receiver<ProfileFacts>) {
        let desktop = self.clone();
        let weak: Weak<Cores> = Arc::downgrade(cores);
        let (mut pool, mut scheduler) = cores.changes();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    published = pool.changed() => if published.is_err() { return },
                    Ok(()) = scheduler.changed() => {}
                    // A closed store publishes nothing more: its branch is skipped.
                    Ok(()) = facts.changed() => {}
                }
                let Some(cores) = weak.upgrade() else { return };
                desktop.update(&cores, &facts);
            }
        });
    }
}
