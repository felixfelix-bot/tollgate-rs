//! Things that happened, as the host reports them.
//!
//! Core never observes anything itself. The host turns sockets, timers, wallet
//! callbacks and meter readings into these, feeds them in, and executes the
//! [`Action`](crate::Action)s that come back.

use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, Message, PubKey};

use crate::meter::Counters;

/// An input to the session state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A peer's transport came up and the layer underneath authenticated it.
    /// Nothing is delivered for it yet.
    PeerConnected {
        /// The peer.
        peer: PubKey,
    },

    /// A peer's transport went away, cleanly or otherwise.
    PeerDisconnected {
        /// The peer.
        peer: PubKey,
    },

    /// A TollGate message arrived.
    ///
    /// **The host has already verified any signature it carries.** Core decides
    /// what is owed, never whether the peer is who it claims to be — exactly as
    /// FIPS terminates Noise before the protocol layer sees anything.
    MessageReceived {
        /// Who sent it.
        peer: PubKey,
        /// What they sent.
        msg: Message,
    },

    /// Our wallet finished funding the channel we pay this peer on, in response
    /// to [`Action::FundChannel`](crate::Action::FundChannel).
    OutgoingChannelFunded {
        /// The peer we will pay on it.
        peer: PubKey,
        /// The channel.
        channel_id: ChannelId,
        /// Units it can carry before it must roll over.
        capacity: u64,
        /// Opaque funding blob to put in the Accept, interpreted by whatever
        /// channel backend produced it.
        funding: Vec<u8>,
    },

    /// Our wallet verified the funding a peer sent, in response to
    /// [`Action::VerifyFunding`](crate::Action::VerifyFunding).
    IncomingFundingVerified {
        /// The peer that funded it.
        peer: PubKey,
        /// The channel they will pay us on.
        channel_id: ChannelId,
        /// Units it can carry.
        capacity: u64,
    },

    /// Our wallet could not verify a peer's funding.
    IncomingFundingRejected {
        /// The peer whose funding failed.
        peer: PubKey,
    },

    /// Fresh cumulative meter readings for a peer.
    Metered {
        /// The peer.
        peer: PubKey,
        /// Cumulative units since session start, both directions.
        counters: Counters,
    },

    /// What this node currently wants to move over a link, in units per second.
    ///
    /// This is what drives [`buyer`](crate::buyer). How it is measured is the
    /// host's business — offered load, recent throughput, queue depth.
    DemandObserved {
        /// The peer we would be buying from.
        peer: PubKey,
        /// Units per second wanted.
        rate: u64,
    },

    /// Time passed. Drives deadline expiry, renewals and rollover.
    Tick,
}
