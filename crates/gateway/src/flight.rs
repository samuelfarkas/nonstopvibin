//! Per-profile in-flight accounting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use nsv_domain::id::ProfileId;
use nsv_domain::sync::locked;

pub(crate) const LIMIT: u32 = 32;

/// Counts requests per profile. A profile with none has no entry, so the map stays bounded.
#[derive(Debug, Default)]
pub(crate) struct Counts(Mutex<HashMap<ProfileId, u32>>);

impl Counts {
    /// Counts a request in: whether it is the profile's first, or `None` at [`LIMIT`].
    pub(crate) fn open(&self, profile: &ProfileId) -> Option<bool> {
        let mut counts = self.lock();
        let count = counts.entry(profile.clone()).or_insert(0);
        if *count >= LIMIT {
            return None;
        }
        *count += 1;
        Some(*count == 1)
    }

    /// Counts a request out: whether it was the profile's last.
    pub(crate) fn close(&self, profile: &ProfileId) -> bool {
        let mut counts = self.lock();
        let Some(count) = counts.get_mut(profile) else {
            return true;
        };
        *count = count.saturating_sub(1);
        let last = *count == 0;
        if last {
            counts.remove(profile);
        }
        last
    }

    pub(crate) fn get(&self, profile: &ProfileId) -> u32 {
        self.lock().get(profile).copied().unwrap_or(0)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ProfileId, u32>> {
        locked(&self.0)
    }
}

/// What counts a request out when its [`InFlight`] drops.
pub(crate) trait Release: Send + Sync {
    fn release(&self, profile: &ProfileId);
}

/// One counted request. Dropping it counts the request out, exactly once, however the
/// request ends: completion, an error, or the agent hanging up.
pub(crate) struct InFlight {
    owner: Arc<dyn Release>,
    profile: ProfileId,
}

impl InFlight {
    pub(crate) fn new(owner: Arc<dyn Release>, profile: ProfileId) -> Self {
        Self { owner, profile }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.owner.release(&self.profile);
    }
}
