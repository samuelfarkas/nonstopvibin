//! Ids, wire types, the provider and agent registry, secret newtypes and the window-helper
//! protocol. No I/O.
#![forbid(unsafe_code)]

pub mod error;
pub mod helper;
pub mod id;
pub mod registry;
pub mod secret;
pub mod time;
pub mod wire;
