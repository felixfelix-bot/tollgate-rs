//! Sans-IO TollGate protocol logic.
//!
//! This crate is **pure**: it never does I/O, never reads the clock, never
//! verifies a signature, and depends on no async runtime. The host
//! (`tollgate-net`) turns real events into [`Event`] values, feeds them in, and
//! executes the [`Action`]s that come back. Every decision that needs the time
//! takes it as a `now_ms` argument.
//!
//! That boundary is what lets the same logic run on a Linux router and on an
//! ESP32: the crate is `no_std` + `alloc`, and the parts that would need a
//! runtime live entirely on the host's side of the seam.
//!
//! Module layout follows the same split throughout — `core.rs` holds the pure
//! decisions, `state.rs` the data they read, `limits.rs` the arithmetic:
//!
//! - [`grant`] — what a peer bought and what it has drawn down. The provider
//!   side of payment.
//! - [`buyer`] — when to buy and how much. The payer side, including the demand
//!   -tracking algorithm that raises the purchased rate as traffic climbs.
//! - [`session`] — the per-peer message lifecycle that ties the two together.
//! - [`meter`] — cumulative delivered/received counters, link-local.
//! - [`access`] — the delivery gate.
//!
//! # What the host must do before calling in
//!
//! Core trusts what it is handed. Specifically, the host must have
//! **verified any signature** on a message before wrapping it in
//! [`Event::MessageReceived`], exactly as FIPS terminates Noise before the
//! protocol layer sees a peer. Core decides *what* is owed and *when*; it never
//! decides whether a peer is who it claims to be.

#![no_std]

extern crate alloc;

pub mod access;
pub mod buyer;
pub mod config;
pub mod grant;
pub mod meter;
pub mod session;

mod action;
mod event;
mod time;

pub use access::AccessLevel;
pub use action::Action;
pub use config::{GrantPolicy, NodePolicy, PeerPolicy};
pub use event::Event;
pub use time::Millis;

// Re-exported so a host can name the wire types without also depending on
// `tollgate-protocol` directly.
pub use tollgate_protocol::{ChannelId, Message, PubKey, ReasonCode, Signature};
