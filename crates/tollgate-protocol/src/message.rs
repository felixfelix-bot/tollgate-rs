//! One struct per message type, plus the [`Message`] union the codec works on.

use alloc::string::String;
use alloc::vec::Vec;

use crate::types::{ChannelId, MsgType, PubKey, ReasonCode, Signature};

/// `0x00` — first message each peer sends. Peers are already authenticated by
/// the layer underneath, so this identifies rather than authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// Protocol version. Must match on both sides.
    pub version: u8,
    /// Sender's compressed secp256k1 public key.
    pub pubkey: PubKey,
    /// Quantity unit this node denominates in — `"byte"`, `"wh"`, `"ml"`.
    pub unit: String,
    /// Capability bitfield. All bits reserved in v1.
    pub capabilities: u32,
}

/// `0x01` — what this node will take payment in, and how welcome the peer's
/// uploads are. Carries no price: delivery is one voucher per unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Mints whose vouchers this node accepts, most preferred first. Never
    /// empty — a node that will take no payment has nothing to offer.
    pub accepted_mints: Vec<String>,
    /// Quantity unit, matching [`Announce::unit`].
    pub unit: String,
    /// Smallest grant window this node will accept, in milliseconds. Bounds how
    /// many signature verifications a payer can impose per second.
    pub min_window_ms: u32,
    /// Largest grant window, in milliseconds. Bounds how far ahead capacity can
    /// be bought.
    pub max_window_ms: u32,
    /// Unsigned surcharge on units received from the peer. `0` is no surcharge;
    /// the net rate on the peer's upload is `m - 1`.
    pub received_multiplier: u16,
}

/// `0x02` — accept the offer and fund the outgoing channel. Nothing is echoed
/// back: the payer picks a mint from the list the Offer already carried, and
/// the window is chosen per grant rather than agreed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    /// Opaque channel-funding blob, interpreted by the channel backend. Empty
    /// for free peering, where neither side charges and no channel exists.
    pub funding: Vec<u8>,
}

/// `0x03` — the funding verified and the channel is active. Sent by whichever
/// party verified the funding, which is the party that will be paid on that
/// channel, so the direction needs no field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelReady {
    /// The channel now active.
    pub channel_id: ChannelId,
}

/// One channel's ratchet turn.
///
/// Signed on its own, over `(channel_id, cumulative)` and nothing else — the
/// window is deliberately outside the signature, since it is measured from
/// receipt and the two sides need no clock agreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelUpdate {
    /// The payer's channel this update ratchets.
    pub channel_id: ChannelId,
    /// Total units authorized on that channel, ever. Strictly increasing.
    pub cumulative: u64,
    /// Schnorr signature over `(channel_id, cumulative)`.
    pub signature: Signature,
}

/// `0x04` — the balance updates and the purchase of a rate in one message, and
/// the only payment message in the protocol.
///
/// **The grant is the combined increase across every update.** One message
/// carries as many channels as the payer wants to draw from, which is what lets
/// a purchase span a channel that is filling up and its replacement, and what
/// lets a payer holding vouchers from several accepted mints spend from more
/// than one at a time. The unit is the same whoever issued it; only the issuer
/// differs.
///
/// **Applied atomically.** If any update fails to increase, the whole message
/// is refused — a partial application would leave the grant size ambiguous.
///
/// Each `cumulative` is monotonic on its own channel, which makes this
/// idempotent: a lost message costs nothing because the next one carries the
/// correct totals, and a reordered one is discarded. So it needs no
/// acknowledgment, and a payer may raise its rate and start using it without
/// waiting a round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopUp {
    /// The channels being ratcheted. At least one, at most
    /// [`MAX_CHANNEL_UPDATES`](crate::MAX_CHANNEL_UPDATES).
    pub updates: Vec<ChannelUpdate>,
    /// Spend the grant within this long, measured from receipt — so the two
    /// sides need no clock agreement.
    pub window_ms: u32,
}

/// `0x05` — the provider will not honor a grant, most often because the rate
/// would oversubscribe capacity already committed elsewhere.
///
/// Declining to ratchet already leaves the payer's money untouched; this exists
/// so the payer learns in one round trip instead of inferring it from
/// throughput that never arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopUpReject {
    /// The states we are declining to ratchet to, echoed back so the payer can
    /// tell which purchase was refused. A single channel no longer identifies
    /// one, since a purchase may span several. Signatures are not echoed —
    /// the payer already holds them and this message is rare.
    pub refused: Vec<RefusedUpdate>,
    /// Units per second we would accept, so the payer can re-purchase at once.
    pub max_rate_available: u64,
    /// Why it was refused.
    pub reason: ReasonCode,
}

/// One entry of a refused purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusedUpdate {
    /// The channel.
    pub channel_id: ChannelId,
    /// The cumulative total we did not ratchet to.
    pub cumulative: u64,
}

/// `0x06` — open a new channel alongside one approaching exhaustion. Initiated
/// by the funder alone: only the party putting up new funds decides when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloverInit {
    /// The channel being replaced, which keeps draining to 100%.
    pub old_channel_id: ChannelId,
    /// Funding blob for the replacement channel.
    pub funding: Vec<u8>,
}

/// `0x07` — the replacement channel is funded and ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloverReady {
    /// The channel being replaced.
    pub old_channel_id: ChannelId,
    /// The replacement, which grants continue on once the old one exhausts.
    pub new_channel_id: ChannelId,
}

/// Why a channel is being closed. A distinct set from [`ReasonCode`] — these
/// describe an orderly wind-down rather than a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CloseReason {
    /// Ordinary cooperative close.
    Normal = 0,
    /// The peer's revised multiplier was not acceptable.
    PriceRejected = 1,
    /// The peer is going away.
    PeerLeaving = 2,
}

impl CloseReason {
    /// Map a wire code onto a reason, defaulting unknown codes to
    /// [`Self::Normal`] — the close proceeds either way.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::PriceRejected,
            2 => Self::PeerLeaving,
            _ => Self::Normal,
        }
    }
}

/// `0x08` — request a cooperative close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelClose {
    /// The channel to close.
    pub channel_id: ChannelId,
    /// Proposed final balance.
    pub final_balance: u64,
    /// Signature over the final balance.
    pub final_signature: Signature,
    /// Why.
    pub reason: CloseReason,
}

/// `0x09` — acknowledge a close at the agreed balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseAck {
    /// The channel being closed.
    pub channel_id: ChannelId,
    /// The final balance both sides settle on.
    pub accepted_balance: u64,
}

/// `0x0A` — general-purpose rejection for any proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    /// The wire tag of the message being rejected.
    pub rejected_type: u8,
    /// Machine-readable reason.
    pub reason: ReasonCode,
    /// Optional human-readable detail.
    pub text: Option<String>,
}

/// `0x0B` — orderly teardown of the whole relationship. A bare FIN instead is
/// treated as an unclean disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    /// Why we are leaving.
    pub reason: ReasonCode,
}

/// Any TollGate message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// See [`Announce`].
    Announce(Announce),
    /// See [`Offer`].
    Offer(Offer),
    /// See [`Accept`].
    Accept(Accept),
    /// See [`ChannelReady`].
    ChannelReady(ChannelReady),
    /// See [`TopUp`].
    TopUp(TopUp),
    /// See [`TopUpReject`].
    TopUpReject(TopUpReject),
    /// See [`RolloverInit`].
    RolloverInit(RolloverInit),
    /// See [`RolloverReady`].
    RolloverReady(RolloverReady),
    /// See [`ChannelClose`].
    ChannelClose(ChannelClose),
    /// See [`CloseAck`].
    CloseAck(CloseAck),
    /// See [`Reject`].
    Reject(Reject),
    /// See [`Disconnect`].
    Disconnect(Disconnect),
}

impl Message {
    /// The wire tag this message encodes under.
    pub fn msg_type(&self) -> MsgType {
        match self {
            Self::Announce(_) => MsgType::Announce,
            Self::Offer(_) => MsgType::Offer,
            Self::Accept(_) => MsgType::Accept,
            Self::ChannelReady(_) => MsgType::ChannelReady,
            Self::TopUp(_) => MsgType::TopUp,
            Self::TopUpReject(_) => MsgType::TopUpReject,
            Self::RolloverInit(_) => MsgType::RolloverInit,
            Self::RolloverReady(_) => MsgType::RolloverReady,
            Self::ChannelClose(_) => MsgType::ChannelClose,
            Self::CloseAck(_) => MsgType::CloseAck,
            Self::Reject(_) => MsgType::Reject,
            Self::Disconnect(_) => MsgType::Disconnect,
        }
    }
}
