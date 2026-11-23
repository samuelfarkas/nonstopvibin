//! Integration tests: the golden replay, pass-through behaviour, and the CPU benchmark.
// `cfg(test)` is always set here; it marks the modules as test code for clippy.

#[cfg(test)]
mod common;
#[cfg(test)]
mod golden;
#[cfg(test)]
mod proxy;
