//! Panics. The hook logs where one happened, never its payload, which may hold request data.
//!
//! A panic that someone is waiting on is that caller's error: a request's connection task ends
//! that request, and a blocking task's caller receives its `JoinError`. Any other panic (the
//! pool's owner, the scheduler, the store thread) leaves state no one will repair, so it starts
//! the daemon's graceful shutdown, which still stops the cores, and the process exits non-zero.

use std::io;
use std::panic::PanicHookInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

tokio::task_local! {
    static REQUEST: ();
}

/// The name of [`runtime`]'s blocking threads, whose panics their callers receive.
const BLOCKING_THREAD: &str = "nsv-blocking";

static FATAL: Notify = Notify::const_new();
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Installs the hook. Call once, first thing in `main`.
pub fn install_hook() {
    std::panic::set_hook(Box::new(hook));
}

/// The runtime the daemon and every command run on: one thread, and at most four blocking
/// threads that exit after two idle seconds.
///
/// # Errors
/// When the runtime's I/O driver cannot start.
pub fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(4)
        .thread_keep_alive(Duration::from_secs(2))
        .thread_name(BLOCKING_THREAD)
        .build()
}

fn hook(info: &PanicHookInfo<'_>) {
    let location = info.location().map_or_else(
        || "an unknown location".to_owned(),
        |at| format!("{}:{}", at.file(), at.line()),
    );
    let waited_on =
        REQUEST.try_with(|()| ()).is_ok() || std::thread::current().name() == Some(BLOCKING_THREAD);
    if waited_on {
        tracing::error!("a request failed with a panic at {location}");
    } else {
        tracing::error!("panic at {location}; shutting down");
        PANICKED.store(true, Ordering::Release);
        FATAL.notify_one();
    }
}

/// Runs a connection's requests so that a panic in them ends only that connection.
pub(crate) async fn request<T>(work: impl Future<Output = T>) -> T {
    REQUEST.scope((), work).await
}

/// Resolves once a panic outside any request happened.
pub async fn fatal() {
    if !PANICKED.load(Ordering::Acquire) {
        FATAL.notified().await;
    }
}

#[must_use]
pub fn panicked() -> bool {
    PANICKED.load(Ordering::Acquire)
}
