//! Payment channels, behind a trait.
//!
//! Channels exist to keep the issuer's spent-proof set bounded, not to prevent
//! theft: when the provider is the mint for its own vouchers, the only party
//! who could cheat is also the party who would have to honor the refund. What
//! bounds that is exposure and reputation, not cryptography. Without channels
//! the spent-proof set is roughly 700× larger, which ends a session within
//! hours on hardware with 16–128 MB of flash.
//!
//! The trait is where a real Cashu Spilman implementation plugs in. It is
//! deliberately synchronous: the reference implementation's own in-memory mint
//! path is synchronous too, and the node drives every call through
//! `spawn_blocking`, feeding the result back as an [`Event`] — which is exactly
//! the seam core's action/event split exists to provide.
//!
//! [`Event`]: tollgate_core::Event

use anyhow::Result;
use tollgate_protocol::{ChannelId, PubKey, Signature};

mod local;
mod spilman;

pub use local::LocalChannels;
pub use spilman::{Money, SpilmanChannels, SpilmanConfig};

/// A channel we funded, ready to tell the peer about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundedChannel {
    /// The channel.
    pub channel_id: ChannelId,
    /// Units it can carry before it must roll over.
    pub capacity: u64,
    /// Opaque blob for the peer's Accept or RolloverInit. Only the backend on
    /// the other side has to understand it.
    pub funding: Vec<u8>,
}

/// A peer's funding, once we have checked it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedChannel {
    /// The channel they will pay us on.
    pub channel_id: ChannelId,
    /// Units it can carry.
    pub capacity: u64,
}

/// What a payment-channel implementation has to provide.
///
/// Signing lives here rather than beside the node's identity because **what a
/// channel update commits to is the channel scheme's business**. An ad-hoc
/// Schnorr over `(channel_id, cumulative)` and a Cashu Spilman balance-update
/// signature cover different preimages entirely, and core treats the bytes as
/// opaque precisely so that either can be dropped in.
pub trait ChannelBackend: Send + Sync + std::fmt::Debug {
    /// Fund a channel to pay `peer` on, against `mint_url`.
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel>;

    /// Check funding a peer sent us and return the channel it opens.
    fn verify(&self, peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel>;

    /// Sign a ratchet turn on a channel we fund.
    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature>;

    /// Check a peer's ratchet turn on a channel it funds.
    ///
    /// Called before the message reaches core, which trusts what it is handed.
    fn verify_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool;

    /// Settle a channel: submit the latest signed state, reclaim the change.
    fn settle(&self, channel_id: ChannelId) -> Result<()>;
}
