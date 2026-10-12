//! The per-peer lifecycle that ties the provider and payer sides together.
//!
//! - `state.rs` — [`PeerSession`], the two channel slots, and the phase.
//! - `core.rs` — [`Sessions`], which turns an [`Event`](crate::Event) into a
//!   list of [`Action`](crate::Action)s.

mod core;
mod state;

#[cfg(test)]
mod tests;

pub use core::Sessions;
pub use state::{PeerOffer, PeerSession, Phase};
