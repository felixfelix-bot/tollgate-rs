//! The delivery gate.
//!
//! Core decides *what* to enforce; the host's resource adapter decides *how* —
//! a FIPS delivery filter, an nftables rule, a token bucket. The level says only
//! whether delivery is allowed and whether it is metered. How much a peer has
//! left to spend lives in [`crate::grant`] and never surfaces here.

/// A peer's delivery status. Exactly one at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessLevel {
    /// Connected, nothing funded. No delivery; TollGate messages still flow, so
    /// the peer can negotiate its way out of this state.
    #[default]
    None,
    /// Channels funded. Delivery allowed, metered, shaped to what was bought.
    Active,
    /// Neither side charges the other. Delivery allowed and unmetered — a
    /// decision about the relationship, not a price set to zero.
    Free,
    /// Channel exhausted past the rollover timeout. Delivery blocked, but the
    /// peer can still negotiate, so it recovers without reconnecting.
    Suspended,
}

impl AccessLevel {
    /// Whether resources may be delivered for or through this peer.
    pub fn delivery_allowed(self) -> bool {
        matches!(self, Self::Active | Self::Free)
    }

    /// Whether traffic is drawn against a grant. `Free` peers are delivered to
    /// without any grant existing.
    pub fn metered(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether the peer should appear in reachability advertisements.
    ///
    /// In FIPS this is bloom-filter inclusion: advertising a peer we will not
    /// deliver through invites other nodes to route into a blackhole. It is
    /// inferred from the level rather than set separately, so the two can never
    /// disagree.
    pub fn advertise(self) -> bool {
        self.delivery_allowed()
    }
}
