//! Per-peer session state.
//!
//! Two payment streams live here, and they are **independent**: the channel the
//! peer pays us on and the channel we pay the peer on. Different mints,
//! different windows, bought at different moments, neither waiting for the
//! other. That is why nearly everything on this struct comes in pairs.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::PubKey;

use crate::access::AccessLevel;
use crate::buyer::{Buyer, WindowBounds};
use crate::config::PeerPolicy;
use crate::grant::GrantState;
use crate::meter::Meter;
use crate::time::Millis;

/// How far a session has got through the opening sequence.
///
/// Both peers walk it symmetrically — each sends Announce, each sends Offer,
/// each funds the channel it will pay on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Connected. We have sent our Announce and Offer; nothing has come back.
    Opening,
    /// Their Announce and Offer are in. Channels are being funded and verified.
    Establishing,
    /// At least one direction is ready. Delivery is allowed.
    Established,
    /// A Disconnect has been sent or received; the host is tearing down.
    Closing,
}

/// What the peer advertised in its Offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOffer {
    /// Mints it will take payment in, most preferred first.
    pub accepted_mints: Vec<String>,
    /// Its quantity unit.
    pub unit: String,
    /// Window bounds it will accept from us.
    pub bounds: WindowBounds,
    /// Its surcharge on what we push at it. We do not enforce this — the peer
    /// does, against our grant — but it is worth holding for the operator to
    /// see why a link is costing what it costs.
    pub received_multiplier: u16,
}

/// Everything we know about one peer.
#[derive(Debug, Clone)]
pub struct PeerSession {
    /// Who they are.
    pub peer: PubKey,
    /// Where we are in the opening sequence.
    pub phase: Phase,
    /// What they may have delivered for them.
    pub access: AccessLevel,
    /// Operator overrides for this peer.
    pub policy: PeerPolicy,
    /// Their Offer, once it arrives.
    pub offer: Option<PeerOffer>,

    // --- the stream where they pay us -------------------------------------
    /// What they have bought and drawn, and the channels they pay us on. The
    /// channels live here because a purchase may ratchet several at once and
    /// the grant is their combined increase.
    pub grant: GrantState,
    /// Cumulative counters, raw. The multiplier is applied when drawing down.
    pub meter: Meter,

    // --- the stream where we pay them --------------------------------------
    /// Our side of the stream where we pay them.
    ///
    /// The channels for this direction live here rather than beside `incoming`,
    /// because the payer has to track up to three at once — the one in use, a
    /// confirmed replacement, and one awaiting confirmation — and each carries
    /// its own cumulative total.
    pub buyer: Buyer,
    /// Units per second we last observed ourselves wanting over this link.
    ///
    /// Our *download*. What we have to buy is more than that whenever the peer
    /// surcharges what we push at it — see [`Self::upload_rate`].
    pub demand: u64,
    /// Units per second we have recently been pushing at this peer.
    ///
    /// A peer's `received_multiplier` is applied to this, and it draws down the
    /// grant we bought. A node that uploads heavily to a peer with `m > 0` and
    /// sized its purchases on download alone would be shaped for the
    /// difference.
    pub upload_rate: u64,
    /// When the meter was last sampled, so a delta can become a rate.
    pub last_meter_at: Millis,

    /// Shaping rate last handed to the adapter, so we only emit a change when
    /// it actually changes. A shaper call per meter reading would be a lot of
    /// churn for a number that mostly stays put.
    pub applied_rate: Option<u64>,
    /// When we last heard anything at all from them.
    pub last_seen: Millis,
}

impl PeerSession {
    /// A freshly connected peer: nothing funded, nothing delivered.
    pub fn new(peer: PubKey, policy: PeerPolicy, now: Millis) -> Self {
        Self {
            peer,
            phase: Phase::Opening,
            access: AccessLevel::None,
            policy,
            offer: None,
            grant: GrantState::new(),
            meter: Meter::new(),
            buyer: Buyer::new(),
            demand: 0,
            upload_rate: 0,
            last_meter_at: now,
            applied_rate: None,
            last_seen: now,
        }
    }

    /// Whether the peer has a live grant with us right now.
    pub fn paying(&self, now: Millis) -> bool {
        self.grant.is_live(now)
    }
}
