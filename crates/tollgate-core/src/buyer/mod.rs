//! The payer side: when to buy capacity, and how much.
//!
//! A payer tops up whenever it wants a rate. Because the signed state is
//! cumulative, a TopUp needs no acknowledgment — a lost one costs nothing since
//! the next carries the correct total, and a reordered one is discarded. So the
//! buyer can raise its rate and start using it in the same breath; the worst
//! case is a single round trip of shaping at the old rate.
//!
//! What it gives up is the ability to pay only for what actually arrived. A
//! grant is consumed whether or not packets land, so transit loss is the
//! buyer's cost. That stays measurable one-sided — the payer knows what it
//! bought and what arrived, both from local counters — so the correction needs
//! no protocol at all: buy a smaller grant, or buy somewhere else.
//!
//! - `state.rs` — [`Buyer`], [`BuyerPolicy`], and the [`Purchase`] it emits.
//! - `core.rs` — [`poll`], the pure buy/hold decision.

mod core;
mod state;

#[cfg(test)]
mod tests;

pub use core::{Demand, poll};
pub use state::{Buyer, BuyerPolicy, ChannelBuyer, Leg, Purchase, Trigger, WindowBounds};
