//! Policy the operator sets, in the form core consumes it.
//!
//! This is not the YAML schema — parsing, file cascades and defaults-on-disk
//! are the host's job. These are the already-resolved values core reads when it
//! decides whether to honor a grant and how hard to shape a peer.
//!
//! There is no price anywhere, because delivery has no price: one voucher buys
//! one unit. What a unit costs in money is settled where vouchers are sold.

use alloc::string::String;
use alloc::vec::Vec;

/// Bounds on what a payer may buy in one purchase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPolicy {
    /// Smallest window this node will accept, in milliseconds.
    ///
    /// This is a cap on how *often* a grant can arrive, and therefore on how
    /// many signature verifications a peer can impose per second. On a
    /// constrained provider that is the binding limit, not bandwidth — raise it
    /// on an ESP32.
    pub min_window_ms: u32,
    /// Largest window, in milliseconds.
    ///
    /// Caps how far ahead capacity can be bought, which is what stops a buyer
    /// accumulating off-peak claims and presenting them all at peak.
    pub max_window_ms: u32,
    /// Units per second this node will commit to a single peer, or `None` for
    /// "whatever the link will bear". A TopUp asking for more is refused with
    /// the available rate attached rather than silently shaped.
    pub max_rate: Option<u64>,
}

impl Default for GrantPolicy {
    fn default() -> Self {
        Self {
            // Five verifications per second worst case...
            min_window_ms: 200,
            // ...and no claim held longer than 30 s.
            max_window_ms: 30_000,
            max_rate: None,
        }
    }
}

impl GrantPolicy {
    /// Whether a payer-chosen window falls inside what we advertise.
    pub fn window_acceptable(&self, window_ms: u32) -> bool {
        (self.min_window_ms..=self.max_window_ms).contains(&window_ms)
    }
}

/// This node's own settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePolicy {
    /// Quantity unit — `"byte"` for network forwarding. Fixed by the resource
    /// and identical across every node selling it.
    pub unit: String,
    /// Mints whose vouchers we take payment in, most preferred first. Never
    /// empty. Each entry is that issuer's credit risk, taken on deliberately.
    pub accepted_mints: Vec<String>,
    /// Default surcharge on units received from a peer, before per-peer
    /// overrides. `0` means no surcharge and the base rule stands: each side
    /// simply pays for what it received.
    pub received_multiplier: u16,
    /// Traffic every peer gets without paying, in units per second.
    ///
    /// This is a **rate and the floor of the shaper**, not a stored quantity —
    /// an unused second of it is gone. It is what a peer falls back to when its
    /// grant expires, which is what leaves the link alive enough to carry the
    /// TopUp that revives it. Keep it small: it is given away, and its resale
    /// value is bounded by economics rather than by cryptography.
    pub minimum_flow: u64,
    /// Bounds on what a payer may buy in one purchase.
    pub grants: GrantPolicy,
    /// Units of capacity to open a new outgoing channel with.
    ///
    /// Channels exist to bound the issuer's spent-proof set, not to prevent
    /// theft, so this trades how often a rollover runs against how much is
    /// committed at once.
    pub initial_channel_capacity: u64,
    /// Drop a peer that has sent nothing at all for this long, in
    /// milliseconds. Zero disables it.
    ///
    /// Only silence, not non-payment: a peer that stops buying simply runs out
    /// of grant and falls to the minimum flow allowance, which costs nothing to
    /// hold open and needs no timer.
    pub stale_timeout_ms: u64,
    /// Percentage of channel capacity at which the funder starts a rollover.
    ///
    /// A percentage rather than a fraction so the comparison stays integer —
    /// there is no reason to pull float math onto a device that would rather
    /// not have it.
    pub rollover_threshold_pct: u8,
}

impl Default for NodePolicy {
    fn default() -> Self {
        Self {
            unit: String::from("byte"),
            accepted_mints: Vec::new(),
            received_multiplier: 0,
            minimum_flow: 0,
            grants: GrantPolicy::default(),
            initial_channel_capacity: 0,
            stale_timeout_ms: 60_000,
            rollover_threshold_pct: 80,
        }
    }
}

/// Per-peer overrides on top of [`NodePolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerPolicy {
    /// Do not charge this peer at all.
    ///
    /// One-sided: it controls only whether *we* charge, never whether the peer
    /// charges back. It is also **not transitive** — free for that peer's own
    /// traffic, never free for anything it is merely the nominal beneficiary
    /// of, which would make an uncharged peer a way to launder free transit.
    pub no_charge: bool,
    /// Refuse this peer entirely.
    pub blocked: bool,
    /// Override the node-wide received multiplier for this peer.
    pub received_multiplier: Option<u16>,
}

impl PeerPolicy {
    /// The multiplier in force for this peer.
    pub fn multiplier(&self, node: &NodePolicy) -> u16 {
        self.received_multiplier.unwrap_or(node.received_multiplier)
    }
}
