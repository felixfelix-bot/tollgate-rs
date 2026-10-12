//! Hand-written CBOR encode/decode for [`Message`].
//!
//! Written against `minicbor`'s low-level `Encoder`/`Decoder` rather than the
//! derive macro because the type tag lives *inside* the same map as the payload
//! fields (`{0: <type>, 1: ..., 2: ...}`), which the derive macro cannot
//! express — it would need the tag in an enclosing structure.
//!
//! Decoding is order-independent: a first pass scans for key `0` to learn the
//! type, and a second pass dispatches on the remaining keys. Unknown keys are
//! skipped rather than rejected, so a peer running a later minor version can
//! add fields without breaking us.
//!
//! **Maps must be definite-length.** That is what we emit and what RFC 8949's
//! canonical form prescribes; an indefinite-length map is rejected rather than
//! silently mis-parsed.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use minicbor::{Decoder, Encoder};

use crate::message::{
    Accept, Announce, ChannelClose, ChannelReady, ChannelUpdate, CloseAck, CloseReason, Disconnect,
    Message, Offer, RefusedUpdate, Reject, RolloverInit, RolloverReady, TopUp, TopUpReject,
};
use crate::types::{ChannelId, MsgType, PubKey, ReasonCode, Signature};

/// Something went wrong encoding or decoding a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not well-formed CBOR, or a value had the wrong major type.
    Cbor,
    /// Key `0` held a tag we do not know.
    UnknownType(u8),
    /// Key `0` was absent, so there is no way to tell what this is.
    MissingType,
    /// A field the message type requires was absent.
    MissingField {
        /// The message's own wire tag.
        msg: u8,
        /// The key that should have been present.
        key: u8,
    },
    /// A fixed-width field (pubkey, channel id, signature) had the wrong length.
    BadLength {
        /// The key that carried it.
        key: u8,
        /// How many bytes it should have held.
        expected: usize,
        /// How many it actually held.
        got: usize,
    },
    /// The map used an indefinite length, which this codec does not accept.
    IndefiniteLength,
    /// An Offer carried an empty mint list. A node that will take no payment
    /// has nothing to offer, so this is malformed rather than merely unusual.
    EmptyMintList,
    /// A TopUp carried no channel updates, so it buys nothing.
    NoChannelUpdates,
    /// A TopUp carried more updates than [`crate::MAX_CHANNEL_UPDATES`].
    ///
    /// Each one is a signature verification, and `min_window_ms` bounds how
    /// often a payer may send a TopUp — an unbounded array would multiply
    /// straight through that budget.
    TooManyChannelUpdates(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cbor => write!(f, "malformed CBOR"),
            Self::UnknownType(t) => write!(f, "unknown message type 0x{t:02x}"),
            Self::MissingType => write!(f, "message has no type field"),
            Self::MissingField { msg, key } => {
                write!(f, "message 0x{msg:02x} is missing required key {key}")
            }
            Self::BadLength { key, expected, got } => {
                write!(f, "key {key}: expected {expected} bytes, got {got}")
            }
            Self::IndefiniteLength => write!(f, "indefinite-length map not accepted"),
            Self::EmptyMintList => write!(f, "offer carried an empty mint list"),
            Self::NoChannelUpdates => write!(f, "top-up carried no channel updates"),
            Self::TooManyChannelUpdates(n) => write!(
                f,
                "top-up carried {n} channel updates, more than the {} allowed",
                crate::MAX_CHANNEL_UPDATES
            ),
        }
    }
}

impl core::error::Error for Error {}

impl From<minicbor::decode::Error> for Error {
    fn from(_: minicbor::decode::Error) -> Self {
        Self::Cbor
    }
}

impl<E> From<minicbor::encode::Error<E>> for Error {
    fn from(_: minicbor::encode::Error<E>) -> Self {
        Self::Cbor
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode a message as CBOR, appending to `out`.
///
/// No length prefix — see [`crate::encode_frame`] for the framed form the TCP
/// transport puts on the wire.
pub fn encode(msg: &Message, out: &mut Vec<u8>) -> Result<(), Error> {
    let mut e = Encoder::new(out);
    let tag = msg.msg_type() as u8;

    match msg {
        Message::Announce(m) => {
            e.map(5)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.u8(m.version)?;
            e.u8(2)?.bytes(&m.pubkey.0)?;
            e.u8(3)?.str(&m.unit)?;
            e.u8(4)?.u32(m.capabilities)?;
        }
        Message::Offer(m) => {
            e.map(5)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.array(m.accepted_mints.len() as u64)?;
            for mint in &m.accepted_mints {
                e.str(mint)?;
            }
            e.u8(2)?.str(&m.unit)?;
            e.u8(3)?
                .array(2)?
                .u32(m.min_window_ms)?
                .u32(m.max_window_ms)?;
            e.u8(4)?.u16(m.received_multiplier)?;
        }
        Message::Accept(m) => {
            e.map(2)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.funding)?;
        }
        Message::ChannelReady(m) => {
            e.map(2)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.channel_id.0)?;
        }
        Message::TopUp(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.array(m.updates.len() as u64)?;
            for u in &m.updates {
                e.array(3)?
                    .bytes(&u.channel_id.0)?
                    .u64(u.cumulative)?
                    .bytes(&u.signature.0)?;
            }
            e.u8(2)?.u32(m.window_ms)?;
        }
        Message::TopUpReject(m) => {
            e.map(4)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.array(m.refused.len() as u64)?;
            for r in &m.refused {
                e.array(2)?.bytes(&r.channel_id.0)?.u64(r.cumulative)?;
            }
            e.u8(2)?.u64(m.max_rate_available)?;
            e.u8(3)?.u8(m.reason as u8)?;
        }
        Message::RolloverInit(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.old_channel_id.0)?;
            e.u8(2)?.bytes(&m.funding)?;
        }
        Message::RolloverReady(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.old_channel_id.0)?;
            e.u8(2)?.bytes(&m.new_channel_id.0)?;
        }
        Message::ChannelClose(m) => {
            e.map(5)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.channel_id.0)?;
            e.u8(2)?.u64(m.final_balance)?;
            e.u8(3)?.bytes(&m.final_signature.0)?;
            e.u8(4)?.u8(m.reason as u8)?;
        }
        Message::CloseAck(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.channel_id.0)?;
            e.u8(2)?.u64(m.accepted_balance)?;
        }
        Message::Reject(m) => {
            e.map(4)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.u8(m.rejected_type)?;
            e.u8(2)?.u8(m.reason as u8)?;
            match &m.text {
                Some(t) => {
                    e.u8(3)?.str(t)?;
                }
                None => {
                    e.u8(3)?.null()?;
                }
            }
        }
        Message::Disconnect(m) => {
            e.map(2)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.u8(m.reason as u8)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode one CBOR message.
pub fn decode(input: &[u8]) -> Result<Message, Error> {
    let tag = scan_type(input)?;
    let ty = MsgType::from_u8(tag).ok_or(Error::UnknownType(tag))?;

    let mut d = Decoder::new(input);
    let pairs = map_len(&mut d)?;

    match ty {
        MsgType::Announce => decode_announce(&mut d, pairs).map(Message::Announce),
        MsgType::Offer => decode_offer(&mut d, pairs).map(Message::Offer),
        MsgType::Accept => decode_accept(&mut d, pairs).map(Message::Accept),
        MsgType::ChannelReady => decode_channel_ready(&mut d, pairs).map(Message::ChannelReady),
        MsgType::TopUp => decode_topup(&mut d, pairs).map(Message::TopUp),
        MsgType::TopUpReject => decode_topup_reject(&mut d, pairs).map(Message::TopUpReject),
        MsgType::RolloverInit => decode_rollover_init(&mut d, pairs).map(Message::RolloverInit),
        MsgType::RolloverReady => decode_rollover_ready(&mut d, pairs).map(Message::RolloverReady),
        MsgType::ChannelClose => decode_channel_close(&mut d, pairs).map(Message::ChannelClose),
        MsgType::CloseAck => decode_close_ack(&mut d, pairs).map(Message::CloseAck),
        MsgType::Reject => decode_reject(&mut d, pairs).map(Message::Reject),
        MsgType::Disconnect => decode_disconnect(&mut d, pairs).map(Message::Disconnect),
    }
}

/// Read the map header and return the pair count, rejecting indefinite lengths.
fn map_len(d: &mut Decoder<'_>) -> Result<u64, Error> {
    d.map()?.ok_or(Error::IndefiniteLength)
}

/// First pass: walk the map skipping values until key `0` turns up.
///
/// Costs one extra traversal of a message that is at most a few hundred bytes,
/// and buys order-independence — we never have to assume the sender put the
/// type first.
fn scan_type(input: &[u8]) -> Result<u8, Error> {
    let mut d = Decoder::new(input);
    let pairs = map_len(&mut d)?;
    for _ in 0..pairs {
        let key = d.u8()?;
        if key == 0 {
            return Ok(d.u8()?);
        }
        d.skip()?;
    }
    Err(Error::MissingType)
}

/// Read a byte string of exactly `N` bytes.
fn fixed<const N: usize>(d: &mut Decoder<'_>, key: u8) -> Result<[u8; N], Error> {
    let bytes = d.bytes()?;
    bytes.try_into().map_err(|_| Error::BadLength {
        key,
        expected: N,
        got: bytes.len(),
    })
}

/// The field was required and never showed up.
fn missing<T>(msg: MsgType, key: u8) -> Result<T, Error> {
    Err(Error::MissingField {
        msg: msg as u8,
        key,
    })
}

fn decode_announce(d: &mut Decoder<'_>, pairs: u64) -> Result<Announce, Error> {
    let (mut version, mut pubkey, mut unit, mut capabilities) = (None, None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => version = Some(d.u8()?),
            2 => pubkey = Some(PubKey(fixed::<33>(d, 2)?)),
            3 => unit = Some(d.str()?.to_string()),
            4 => capabilities = Some(d.u32()?),
            _ => d.skip()?,
        }
    }
    Ok(Announce {
        version: match version {
            Some(v) => v,
            None => return missing(MsgType::Announce, 1),
        },
        pubkey: match pubkey {
            Some(v) => v,
            None => return missing(MsgType::Announce, 2),
        },
        unit: match unit {
            Some(v) => v,
            None => return missing(MsgType::Announce, 3),
        },
        // Absent capabilities means "none", which is also what v1 requires.
        capabilities: capabilities.unwrap_or(0),
    })
}

fn decode_offer(d: &mut Decoder<'_>, pairs: u64) -> Result<Offer, Error> {
    let mut mints: Option<Vec<String>> = None;
    let (mut unit, mut windows, mut multiplier) = (None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => {
                let n = d.array()?.ok_or(Error::IndefiniteLength)?;
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    list.push(d.str()?.to_string());
                }
                mints = Some(list);
            }
            2 => unit = Some(d.str()?.to_string()),
            3 => {
                let n = d.array()?.ok_or(Error::IndefiniteLength)?;
                if n != 2 {
                    return Err(Error::BadLength {
                        key: 3,
                        expected: 2,
                        got: n as usize,
                    });
                }
                windows = Some((d.u32()?, d.u32()?));
            }
            4 => multiplier = Some(d.u16()?),
            _ => d.skip()?,
        }
    }
    let accepted_mints = match mints {
        Some(v) => v,
        None => return missing(MsgType::Offer, 1),
    };
    if accepted_mints.is_empty() {
        return Err(Error::EmptyMintList);
    }
    let (min_window_ms, max_window_ms) = match windows {
        Some(v) => v,
        None => return missing(MsgType::Offer, 3),
    };
    Ok(Offer {
        accepted_mints,
        unit: match unit {
            Some(v) => v,
            None => return missing(MsgType::Offer, 2),
        },
        min_window_ms,
        max_window_ms,
        // The multiplier defaults to 0 — no surcharge, the base rule stands.
        received_multiplier: multiplier.unwrap_or(0),
    })
}

fn decode_accept(d: &mut Decoder<'_>, pairs: u64) -> Result<Accept, Error> {
    let mut funding = None;
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => funding = Some(d.bytes()?.to_vec()),
            _ => d.skip()?,
        }
    }
    Ok(Accept {
        // An absent blob is free peering: accepted, but nothing funded.
        funding: funding.unwrap_or_default(),
    })
}

fn decode_channel_ready(d: &mut Decoder<'_>, pairs: u64) -> Result<ChannelReady, Error> {
    let mut channel_id = None;
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => channel_id = Some(ChannelId(fixed::<32>(d, 1)?)),
            _ => d.skip()?,
        }
    }
    Ok(ChannelReady {
        channel_id: match channel_id {
            Some(v) => v,
            None => return missing(MsgType::ChannelReady, 1),
        },
    })
}

fn decode_topup(d: &mut Decoder<'_>, pairs: u64) -> Result<TopUp, Error> {
    let mut updates: Option<Vec<ChannelUpdate>> = None;
    let mut window_ms = None;

    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => {
                let n = d.array()?.ok_or(Error::IndefiniteLength)?;
                // Checked before allocating: each update is a signature
                // verification, so a peer must not be able to name a huge count
                // and have us reserve for it.
                if n as usize > crate::MAX_CHANNEL_UPDATES {
                    return Err(Error::TooManyChannelUpdates(n as usize));
                }
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let fields = d.array()?.ok_or(Error::IndefiniteLength)?;
                    if fields != 3 {
                        return Err(Error::BadLength {
                            key: 1,
                            expected: 3,
                            got: fields as usize,
                        });
                    }
                    list.push(ChannelUpdate {
                        channel_id: ChannelId(fixed::<32>(d, 1)?),
                        cumulative: d.u64()?,
                        signature: Signature(fixed::<64>(d, 1)?),
                    });
                }
                updates = Some(list);
            }
            2 => window_ms = Some(d.u32()?),
            _ => d.skip()?,
        }
    }

    let updates = match updates {
        Some(v) => v,
        None => return missing(MsgType::TopUp, 1),
    };
    // A purchase that ratchets nothing buys nothing.
    if updates.is_empty() {
        return Err(Error::NoChannelUpdates);
    }

    Ok(TopUp {
        updates,
        window_ms: match window_ms {
            Some(v) => v,
            None => return missing(MsgType::TopUp, 2),
        },
    })
}

fn decode_topup_reject(d: &mut Decoder<'_>, pairs: u64) -> Result<TopUpReject, Error> {
    let mut refused: Option<Vec<RefusedUpdate>> = None;
    let (mut max_rate, mut reason) = (None, None);

    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => {
                let n = d.array()?.ok_or(Error::IndefiniteLength)?;
                if n as usize > crate::MAX_CHANNEL_UPDATES {
                    return Err(Error::TooManyChannelUpdates(n as usize));
                }
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let fields = d.array()?.ok_or(Error::IndefiniteLength)?;
                    if fields != 2 {
                        return Err(Error::BadLength {
                            key: 1,
                            expected: 2,
                            got: fields as usize,
                        });
                    }
                    list.push(RefusedUpdate {
                        channel_id: ChannelId(fixed::<32>(d, 1)?),
                        cumulative: d.u64()?,
                    });
                }
                refused = Some(list);
            }
            2 => max_rate = Some(d.u64()?),
            3 => reason = Some(ReasonCode::from_u8(d.u8()?)),
            _ => d.skip()?,
        }
    }

    Ok(TopUpReject {
        refused: match refused {
            Some(v) => v,
            None => return missing(MsgType::TopUpReject, 1),
        },
        max_rate_available: match max_rate {
            Some(v) => v,
            None => return missing(MsgType::TopUpReject, 2),
        },
        reason: reason.unwrap_or(ReasonCode::Other),
    })
}

fn decode_rollover_init(d: &mut Decoder<'_>, pairs: u64) -> Result<RolloverInit, Error> {
    let (mut old_channel_id, mut funding) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => old_channel_id = Some(ChannelId(fixed::<32>(d, 1)?)),
            2 => funding = Some(d.bytes()?.to_vec()),
            _ => d.skip()?,
        }
    }
    Ok(RolloverInit {
        old_channel_id: match old_channel_id {
            Some(v) => v,
            None => return missing(MsgType::RolloverInit, 1),
        },
        funding: match funding {
            Some(v) => v,
            None => return missing(MsgType::RolloverInit, 2),
        },
    })
}

fn decode_rollover_ready(d: &mut Decoder<'_>, pairs: u64) -> Result<RolloverReady, Error> {
    let (mut old_channel_id, mut new_channel_id) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => old_channel_id = Some(ChannelId(fixed::<32>(d, 1)?)),
            2 => new_channel_id = Some(ChannelId(fixed::<32>(d, 2)?)),
            _ => d.skip()?,
        }
    }
    Ok(RolloverReady {
        old_channel_id: match old_channel_id {
            Some(v) => v,
            None => return missing(MsgType::RolloverReady, 1),
        },
        new_channel_id: match new_channel_id {
            Some(v) => v,
            None => return missing(MsgType::RolloverReady, 2),
        },
    })
}

fn decode_channel_close(d: &mut Decoder<'_>, pairs: u64) -> Result<ChannelClose, Error> {
    let (mut channel_id, mut balance, mut signature, mut reason) = (None, None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => channel_id = Some(ChannelId(fixed::<32>(d, 1)?)),
            2 => balance = Some(d.u64()?),
            3 => signature = Some(Signature(fixed::<64>(d, 3)?)),
            4 => reason = Some(CloseReason::from_u8(d.u8()?)),
            _ => d.skip()?,
        }
    }
    Ok(ChannelClose {
        channel_id: match channel_id {
            Some(v) => v,
            None => return missing(MsgType::ChannelClose, 1),
        },
        final_balance: match balance {
            Some(v) => v,
            None => return missing(MsgType::ChannelClose, 2),
        },
        final_signature: match signature {
            Some(v) => v,
            None => return missing(MsgType::ChannelClose, 3),
        },
        reason: reason.unwrap_or(CloseReason::Normal),
    })
}

fn decode_close_ack(d: &mut Decoder<'_>, pairs: u64) -> Result<CloseAck, Error> {
    let (mut channel_id, mut balance) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => channel_id = Some(ChannelId(fixed::<32>(d, 1)?)),
            2 => balance = Some(d.u64()?),
            _ => d.skip()?,
        }
    }
    Ok(CloseAck {
        channel_id: match channel_id {
            Some(v) => v,
            None => return missing(MsgType::CloseAck, 1),
        },
        accepted_balance: match balance {
            Some(v) => v,
            None => return missing(MsgType::CloseAck, 2),
        },
    })
}

fn decode_reject(d: &mut Decoder<'_>, pairs: u64) -> Result<Reject, Error> {
    let (mut rejected_type, mut reason, mut text) = (None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => rejected_type = Some(d.u8()?),
            2 => reason = Some(ReasonCode::from_u8(d.u8()?)),
            3 => {
                if d.datatype()? == minicbor::data::Type::Null {
                    d.null()?;
                } else {
                    text = Some(d.str()?.to_string());
                }
            }
            _ => d.skip()?,
        }
    }
    Ok(Reject {
        rejected_type: match rejected_type {
            Some(v) => v,
            None => return missing(MsgType::Reject, 1),
        },
        reason: reason.unwrap_or(ReasonCode::Other),
        text,
    })
}

fn decode_disconnect(d: &mut Decoder<'_>, pairs: u64) -> Result<Disconnect, Error> {
    let mut reason = None;
    for _ in 0..pairs {
        match d.u8()? {
            0 => {
                d.skip()?;
            }
            1 => reason = Some(ReasonCode::from_u8(d.u8()?)),
            _ => d.skip()?,
        }
    }
    Ok(Disconnect {
        reason: reason.unwrap_or(ReasonCode::Other),
    })
}
