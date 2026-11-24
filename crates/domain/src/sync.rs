use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, recovering it from a panicked holder: every critical section leaves its
/// value consistent, so a panic elsewhere cannot.
pub fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
