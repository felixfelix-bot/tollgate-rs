//! Cumulative delivered/received counters, per peer, link-local.
//!
//! Counts are **not exchanged, not signed, and not an input to any payment**.
//! The payer bought its grant in advance; these are only how the provider knows
//! when that grant is spent. Because no shared number decides how much money
//! moves, there is nothing for the two sides to reconcile and no drift
//! tolerance to configure.
//!
//! The counters stay **raw**. The received multiplier is applied when the grant
//! is drawn down ([`crate::grant`]), not when counting, which keeps what the
//! meter reports separable from what the shaper charges.

/// Cumulative units across a link with one peer, since session start.
///
/// Cumulative rather than deltas because that is what compares directly against
/// `authorized`, the cumulative total the peer has signed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Units delivered **to** this peer — its download.
    pub delivered: u64,
    /// Units received **from** this peer — its upload.
    pub received: u64,
}

impl Counters {
    /// Counters at session start.
    pub const ZERO: Self = Self {
        delivered: 0,
        received: 0,
    };

    /// What grew since `earlier`.
    ///
    /// Saturating, so a counter that resets (an adapter restart, a
    /// re-registered peer) reports no growth for one sample instead of
    /// underflowing into an enormous one.
    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            delivered: self.delivered.saturating_sub(earlier.delivered),
            received: self.received.saturating_sub(earlier.received),
        }
    }

    /// What this delta draws from a grant, given the received multiplier.
    ///
    /// ```text
    /// consumed += delivered + received × received_multiplier
    /// ```
    ///
    /// A unit the peer downloads draws one. A unit it uploads draws `m` — so at
    /// the default `0` its uploads draw nothing and we pay for them out of our
    /// own grant on the other channel, and at `2` an uploaded unit costs the
    /// same as a downloaded one.
    ///
    /// Computed in `u128` and saturated: `received` and `m` are both attacker-
    /// influenced, and wrapping here would hand a peer free capacity.
    pub fn weighted(self, received_multiplier: u16) -> u64 {
        let weighted = self.delivered as u128
            + (self.received as u128).saturating_mul(received_multiplier as u128);
        weighted.min(u64::MAX as u128) as u64
    }
}

/// Tracks a peer's counters and turns each reading into a draw-down.
///
/// The host reports cumulative totals whenever it likes; this holds the last
/// reading so core can work in deltas without the host having to.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meter {
    last: Counters,
}

impl Meter {
    /// A meter at session start.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new cumulative reading and return the weighted draw-down it
    /// implies.
    pub fn observe(&mut self, now: Counters, received_multiplier: u16) -> u64 {
        let delta = now.delta_since(self.last);
        self.last = now;
        delta.weighted(received_multiplier)
    }

    /// The most recent reading.
    pub fn totals(&self) -> Counters {
        self.last
    }
}
