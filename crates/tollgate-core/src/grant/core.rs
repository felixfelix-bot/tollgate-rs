//! Pure admission decisions on an incoming purchase.
//!
//! Snapshot in, verdict out. No clock, no I/O, no state mutated — the caller
//! applies the verdict to a [`GrantState`] if it says to.
//!
//! Admission control is only possible at all because the payer states the rate
//! it wants up front, for a bounded horizon. The provider can sum the rates it
//! has already committed across peers and refuse **before** taking the money,
//! rather than accepting payment and quietly shaping below what was sold.

use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, ChannelUpdate, ReasonCode};

use crate::config::GrantPolicy;
use crate::grant::limits;
use crate::grant::state::GrantState;

/// Everything outside the peer's own grant state that bears on whether a
/// purchase can be honored.
#[derive(Debug, Clone, Copy)]
pub struct Admission<'a> {
    /// Window bounds and the per-peer rate ceiling.
    pub policy: &'a GrantPolicy,
    /// Rate already committed to *other* peers, in units per second. Summed by
    /// the host across its live grants.
    pub committed_elsewhere: u64,
}

impl Admission<'_> {
    /// Rate we could still commit to this peer, given what is committed
    /// elsewhere. `None` in the policy means "whatever the link will bear".
    pub fn rate_available(&self) -> u64 {
        match self.policy.max_rate {
            Some(max) => max.saturating_sub(self.committed_elsewhere),
            None => u64::MAX,
        }
    }
}

/// What to do with an incoming purchase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Honor it. Apply to the grant state and start shaping at `rate`.
    Accept {
        /// Each channel's new cumulative total, ready to apply.
        ratchets: Vec<(ChannelId, u64)>,
        /// Units bought by this purchase alone — the **combined increase**
        /// across every channel it ratchets.
        grant: u64,
        /// Units per second it buys, fixed for the life of the grant.
        rate: u64,
    },
    /// Refuse it, in full.
    ///
    /// Declining to ratchet is already enough to leave the payer's money
    /// untouched — an unclaimed channel state is worth nothing to us. The
    /// refusal is sent so the payer learns in one round trip instead of
    /// inferring it from throughput that never arrived.
    Reject {
        /// Machine-readable cause.
        reason: ReasonCode,
        /// Rate we *would* accept, so the payer can re-purchase immediately
        /// instead of guessing.
        max_rate_available: u64,
    },
}

/// Decide whether to honor a purchase.
///
/// **All or nothing.** Every update must be valid, or the whole message is
/// refused: applying some of them would leave the grant a different size from
/// the one the payer asked for and thought it was paying for.
///
/// The checks:
///
/// 1. every update names a channel we recognise. A settled or replaced channel
///    is not one, which is what stops the recognised set growing with every
///    rollover a long session performs.
/// 2. every `cumulative` increases on its own channel. This is the ratchet, and
///    it is also what makes a replayed or reordered purchase harmless.
/// 3. no channel is signed past its capacity — it could not be settled.
/// 4. the window falls inside what we advertised.
/// 5. the combined rate fits under what we have left to commit.
pub fn evaluate_topup(
    state: &GrantState,
    admission: Admission<'_>,
    updates: &[ChannelUpdate],
    window_ms: u32,
) -> Verdict {
    let available = admission.rate_available();
    let refuse = |reason| Verdict::Reject {
        reason,
        max_rate_available: available,
    };

    if updates.is_empty() {
        return refuse(ReasonCode::GrantInvalid);
    }

    let mut ratchets = Vec::with_capacity(updates.len());
    let mut grant: u64 = 0;

    for update in updates {
        let Some(channel) = state.channel(update.channel_id) else {
            // Either never funded, or already settled. Neither is something we
            // can ratchet.
            return refuse(ReasonCode::FundingInvalid);
        };

        // A cumulative that does not increase is a replay or a reorder.
        // Discarding it is what lets a purchase be fire-and-forget.
        if update.cumulative <= channel.signed {
            return refuse(ReasonCode::GrantInvalid);
        }
        if update.cumulative > channel.capacity {
            return refuse(ReasonCode::GrantExceedsChannel);
        }
        // The same channel twice would let the second reading of `signed` be
        // stale and the delta be counted from the wrong base.
        if ratchets.iter().any(|(id, _)| *id == update.channel_id) {
            return refuse(ReasonCode::GrantInvalid);
        }

        grant = grant.saturating_add(update.cumulative - channel.signed);
        ratchets.push((update.channel_id, update.cumulative));
    }

    if !admission.policy.window_acceptable(window_ms) {
        return refuse(ReasonCode::WindowOutOfRange);
    }

    let rate = limits::rate_from(grant, window_ms);
    if rate > available {
        return refuse(ReasonCode::RateExceedsCapacity);
    }

    Verdict::Accept {
        ratchets,
        grant,
        rate,
    }
}
