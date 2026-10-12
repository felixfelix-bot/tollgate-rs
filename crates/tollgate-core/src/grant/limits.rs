//! Rate arithmetic.
//!
//! A proof holds a quantity and nothing else, so a rate only exists as a
//! quantity paired with a window: `rate = grant / window`. These two functions
//! are that identity in both directions, and everything that converts between a
//! rate and a number of units goes through them.
//!
//! All of it is done in `u128` and saturated back to `u64`. The inputs are
//! attacker-influenced — a peer chooses both the grant and the window — and
//! wrapping arithmetic here would hand it free capacity.

/// Units per second bought by `grant` units spendable over `window_ms`.
///
/// A zero window would be an unbounded rate; it saturates rather than dividing
/// by zero. Policy rejects it long before this is reached, since
/// `min_window_ms` is what bounds signature verifications per second.
pub fn rate_from(grant: u64, window_ms: u32) -> u64 {
    if window_ms == 0 {
        return u64::MAX;
    }
    let rate = (grant as u128) * 1_000 / (window_ms as u128);
    rate.min(u64::MAX as u128) as u64
}

/// Units drawn at `rate` over `ms` milliseconds.
///
/// The inverse of [`rate_from`], up to integer truncation.
pub fn units_in(rate: u64, ms: u64) -> u64 {
    let units = (rate as u128) * (ms as u128) / 1_000;
    units.min(u64::MAX as u128) as u64
}

/// Grant size needed to buy `rate` for `window_ms` — what a payer puts in a
/// TopUp once it has decided what rate it wants.
pub fn grant_for(rate: u64, window_ms: u32) -> u64 {
    units_in(rate, window_ms as u64)
}
