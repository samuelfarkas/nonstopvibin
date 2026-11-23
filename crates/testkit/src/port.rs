#![expect(
    clippy::missing_panics_doc,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicU32, Ordering};

/// The reserved range, 20001 to 32000: above 20000, below the system's ephemeral range.
const FIRST: u32 = 20_001;
const COUNT: u32 = 12_000;
/// The ports the recorded goldens bind or expect to find closed.
const GOLDEN: [u16; 3] = [24_318, 24_319, 24_399];

/// A loopback port for a process that takes a port number, held by a listener until
/// [`Port::release`] hands the number over.
///
/// A port from the system's ephemeral range (from 32768 on Linux, 49152 on macOS) bound with
/// port 0 and released can meanwhile be handed to any other `bind(0)` or outgoing connection.
/// Below that range a free port stays free until something binds that number, and while this
/// listener holds it nothing can. Each port is reserved once per process, and none is one the
/// goldens record. `tests/support/ports.ts` does the same for the TypeScript tests.
pub struct Port {
    number: u16,
    _listener: TcpListener,
}

impl Port {
    #[must_use]
    pub fn reserve() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Each process starts at its own place in the range, so concurrent runs seldom meet.
        let start = std::process::id().wrapping_mul(7_919);
        for _ in 0..COUNT {
            let offset = start.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed)) % COUNT;
            let number = u16::try_from(FIRST + offset).unwrap();
            if GOLDEN.contains(&number) {
                continue;
            }
            if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, number)) {
                return Self {
                    number,
                    _listener: listener,
                };
            }
        }
        panic!(
            "no loopback port from {FIRST} to {} is free",
            FIRST + COUNT - 1
        );
    }

    #[must_use]
    pub fn number(&self) -> u16 {
        self.number
    }

    /// Closes the listener, for the process about to bind the port.
    #[must_use]
    pub fn release(self) -> u16 {
        self.number
    }
}
