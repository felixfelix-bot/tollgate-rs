//! The provider side of payment: what a peer bought, and what it has drawn.
//!
//! A payment is a **grant** — this many units, spendable within this window,
//! starting when the provider receives it. The rate it buys is one divided by
//! the other, and it is fixed for the grant's life. Buying again *replaces* the
//! grant in force rather than adding to it, so whatever was left of the old one
//! is forfeit at that moment.
//!
//! That forfeiture is the whole reason the product is bandwidth rather than a
//! stored quantity of units: capacity is perishable, a second of it that goes
//! unsold is gone whether or not anyone paid, and the buyer carries that same
//! risk on the seconds it bought.
//!
//! - `limits.rs` — the `rate = grant / window` arithmetic, saturating.
//! - `state.rs` — [`GrantState`], the three numbers and their transitions.
//! - `core.rs` — [`evaluate_topup`], the pure admission decision.

mod core;
mod limits;
mod state;

#[cfg(test)]
mod tests;

pub use core::{Admission, Verdict, evaluate_topup};
pub use limits::{grant_for, rate_from, units_in};
pub use state::{GrantState, IncomingChannel};
