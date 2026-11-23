//! The panic hook's verdicts, in a binary of its own because the hook is process-wide: a
//! blocking task's panic is its caller's error, while a background task's starts shutdown.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

use std::time::Duration;

use nsv_server::panic;

#[test]
fn only_a_panic_no_one_waits_on_starts_the_shutdown() {
    panic::install_hook();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .thread_name(panic::BLOCKING_THREAD)
        .build()
        .unwrap();
    runtime.block_on(async {
        // A blocking task's caller receives the panic as its JoinError.
        let joined = tokio::task::spawn_blocking(|| panic!("blocking")).await;
        assert!(joined.unwrap_err().is_panic());
        let waited = tokio::time::timeout(Duration::from_millis(100), panic::fatal()).await;
        assert!(
            waited.is_err(),
            "a blocking task's panic started the shutdown"
        );
        assert!(!panic::panicked());

        // A background task's panic has no one to report to.
        drop(tokio::spawn(async { panic!("background") }));
        tokio::time::timeout(Duration::from_secs(1), panic::fatal())
            .await
            .expect("a background task's panic did not start the shutdown");
        assert!(panic::panicked());
    });
}
