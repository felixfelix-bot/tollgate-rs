//! The pure buy/hold decision.
//!
//! Snapshot in, decision out. The host measures demand and supplies the clock;
//! this decides only whether to buy and how much.
//!
//! The rule is deliberately rudimentary — three cases, no control theory:
//!
//! 1. **Nothing bought yet** — buy for the demand we see.
//! 2. **The grant is about to lapse** — renew. Costs nothing; the remainder was
//!    nearly gone anyway.
//! 3. **Demand climbed a lot** — buy now and eat the forfeit. Only past a
//!    threshold, because raising the rate early burns whatever is left of the
//!    grant in force. The design's own table is the argument for hysteresis:
//!    a large jump costs ~2.5% of the new grant, a small one ~67%.
//!
//! Anything else holds.

use crate::buyer::state::{Buyer, BuyerPolicy, Leg, Purchase, Trigger, WindowBounds};
use crate::grant::grant_for;
use crate::time::Millis;

/// What the host observed since the last decision.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    /// Units per second the host currently wants to move over this link.
    ///
    /// How it is measured is the host's business — offered load, recent
    /// throughput, queue depth. The demo measures what the traffic generator is
    /// asking for.
    pub observed_rate: u64,
    /// Window bounds the provider advertised in its Offer.
    pub bounds: WindowBounds,
}

/// Decide whether to buy.
///
/// Returns `None` to hold. The caller sends a TopUp per leg and then calls
/// [`Buyer::record`] — the two are separate so a host that fails to send does
/// not advance its own ratchet past what the provider saw.
pub fn poll(buyer: &Buyer, policy: &BuyerPolicy, demand: Demand, now: Millis) -> Option<Purchase> {
    let active = buyer.active()?;

    let target = target_rate(buyer, policy, demand.observed_rate, now);
    let window_ms = demand.bounds.clamp(policy.window_ms);

    let trigger = if !buyer.started {
        Trigger::First
    } else if buyer.cap(now).is_some() && target != buyer.rate {
        // The provider named a rate it would take; go straight back with it
        // rather than waiting out a grant we never got.
        Trigger::Rebuy
    // Against the window actually in force, not the one asked for: a provider
    // that caps the window shorter than the configured lead would otherwise put
    // the buyer in a renewal loop.
    } else if now + policy.lead_within(window_ms) as u64 >= buyer.deadline {
        Trigger::Renewal
    } else if worth_the_forfeit(buyer.rate, target, policy) {
        Trigger::DemandRose
    } else {
        return None;
    };

    let wanted = grant_for(target, window_ms);

    // A grant that overflows the channel in use is signed across both: the
    // first is topped to exactly its capacity, and the remainder starts the
    // replacement. A cumulative total only means anything against the channel
    // it was signed on, so this cannot be one message.
    let (first, second, grant) = if wanted <= active.headroom() {
        (
            Leg {
                channel_id: active.id,
                cumulative: active.cumulative + wanted,
            },
            None,
            wanted,
        )
    } else if let Some(next) = buyer.next_channel() {
        let overflow = wanted - active.headroom();
        // The replacement is the same size as the one it replaces, so this only
        // binds if a single grant is larger than a whole channel — in which
        // case buying what fits is the best available answer.
        let overflow = overflow.min(next.headroom());
        (
            Leg {
                channel_id: active.id,
                cumulative: active.capacity,
            },
            Some(Leg {
                channel_id: next.id,
                cumulative: next.cumulative + overflow,
            }),
            active.headroom() + overflow,
        )
    } else {
        // No replacement confirmed yet. Buy what the channel in use will still
        // take; the rollover that is already under way opens the rest.
        (
            Leg {
                channel_id: active.id,
                cumulative: active.capacity,
            },
            None,
            active.headroom(),
        )
    };

    // Buying nothing is not a purchase. This covers an idle link with
    // `min_rate: 0`, and a channel with no headroom left and no replacement.
    if grant == 0 {
        return None;
    }

    Some(Purchase {
        first,
        second,
        window_ms,
        // What was actually bought may be less than the target asked for, when
        // a channel ran out mid-purchase. Report the rate this grant really
        // buys, since that is what the provider will shape to.
        rate: rate_of(grant, window_ms, target, wanted),
        grant,
        forfeited: buyer.unspent_at(now),
        trigger,
    })
}

/// The rate a grant buys, given that a channel boundary may have truncated it.
fn rate_of(grant: u64, window_ms: u32, target: u64, wanted: u64) -> u64 {
    if grant == wanted {
        target
    } else {
        crate::grant::rate_from(grant, window_ms)
    }
}

/// The rate we would like, given demand, headroom, and any ceiling the provider
/// has told us about.
fn target_rate(buyer: &Buyer, policy: &BuyerPolicy, observed: u64, now: Millis) -> u64 {
    let with_headroom = (observed as u128) * (policy.headroom_pct as u128) / 100;
    let mut target = with_headroom.min(u64::MAX as u128) as u64;

    target = target.clamp(policy.min_rate, policy.max_rate);

    // A provider that refused us has already said what it will take. Asking for
    // more again would just be refused again.
    if let Some(cap) = buyer.cap(now) {
        target = target.min(cap);
    }
    target
}

/// Whether the jump from `current` to `target` clears the hysteresis threshold.
///
/// Only upward: demand falling is not a reason to buy at all, since the cheaper
/// grant would still forfeit the expensive one's remainder. We simply let the
/// current grant run out and renew lower.
fn worth_the_forfeit(current: u64, target: u64, policy: &BuyerPolicy) -> bool {
    if target <= current {
        return false;
    }
    let scaled = (current as u128) * (policy.raise_threshold_pct as u128) / 100;
    (target as u128) >= scaled
}
