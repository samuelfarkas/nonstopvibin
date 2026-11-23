//! The built `nonstopvibin` as a process, in one test binary: the command-line goldens, `serve`
//! with the real pinned core, and a start that cannot bind its port.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod golden;
mod serve;
mod startup;
