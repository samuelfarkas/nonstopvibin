//! What every part of the daemon reads, apart from the daemon that assembles them.

use nsv_domain::endpoint::Origin;

use crate::desktop::Desktop;
use crate::failure::Errors;

/// This build's version, as `package.json` states it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What every part of the daemon reads: where it listens, its clock, the error list and the
/// desktop's part.
pub(crate) struct Shared {
    pub(crate) port: u16,
    /// `http://127.0.0.1:<port>`.
    pub(crate) origin: Origin,
    pub(crate) clock: fn() -> i64,
    pub(crate) errors: Errors,
    pub(crate) desktop: Option<Desktop>,
}
