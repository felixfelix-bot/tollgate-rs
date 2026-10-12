//! The buyer algorithm: buys when demand justifies it, holds when the forfeit
//! would not be worth it, never asks for more than the provider said it would
//! take, and carries a purchase across a channel boundary rather than stalling
//! at one.

use tollgate_protocol::ChannelId;

use super::*;
use crate::grant::grant_for;
use crate::time::Millis;

const CAPACITY: u64 = 1_000_000_000;

fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

fn bounds() -> WindowBounds {
    WindowBounds {
        min_ms: 200,
        max_ms: 30_000,
    }
}

fn policy() -> BuyerPolicy {
    BuyerPolicy {
        headroom_pct: 125,
        raise_threshold_pct: 150,
        renew_lead_ms: 500,
        cap_hold_ms: 10_000,
        window_ms: 2_000,
        min_rate: 0,
        max_rate: u64::MAX,
    }
}

fn demand(rate: u64) -> Demand {
    Demand {
        observed_rate: rate,
        bounds: bounds(),
    }
}

/// A buyer with one confirmed channel, which is where every test starts.
fn opened(capacity: u64) -> Buyer {
    let mut buyer = Buyer::new();
    buyer.funded(channel(1), capacity);
    buyer.confirmed(channel(1));
    buyer
}

// ---------------------------------------------------------------------------
// Buying
// ---------------------------------------------------------------------------

#[test]
fn nothing_is_bought_before_a_channel_is_confirmed() {
    // A grant signed against a channel the peer has not verified could be
    // against one that never opens.
    let mut buyer = Buyer::new();
    assert!(poll(&buyer, &policy(), demand(100_000), Millis(0)).is_none());

    buyer.funded(channel(1), CAPACITY);
    assert!(
        poll(&buyer, &policy(), demand(100_000), Millis(0)).is_none(),
        "funded is not the same as confirmed"
    );

    buyer.confirmed(channel(1));
    assert!(poll(&buyer, &policy(), demand(100_000), Millis(0)).is_some());
}

#[test]
fn the_first_purchase_covers_demand_plus_headroom() {
    let buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");

    assert_eq!(p.trigger, Trigger::First);
    assert_eq!(p.rate, 125_000, "100 k/s demand at 125% headroom");
    assert_eq!(p.window_ms, 2_000);
    assert_eq!(p.grant, grant_for(125_000, 2_000));
    assert_eq!(p.first.channel_id, channel(1));
    assert_eq!(
        p.first.cumulative, p.grant,
        "first purchase starts the ratchet"
    );
    assert_eq!(p.second, None, "it fits on the channel in use");
    assert_eq!(p.forfeited, 0, "nothing to give up");
}

#[test]
fn steady_demand_mid_window_holds() {
    // Re-buying the same rate would forfeit the remainder for nothing.
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    for t in [100, 500, 1_000, 1_400] {
        assert!(
            poll(&buyer, &policy(), demand(100_000), Millis(t)).is_none(),
            "should hold at t={t}"
        );
    }
}

#[test]
fn the_grant_is_renewed_before_it_lapses() {
    // Renewing early is what keeps the peer off the minimum flow allowance.
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.deadline(), Millis(2_000));

    assert!(poll(&buyer, &policy(), demand(100_000), Millis(1_499)).is_none());

    let p = poll(&buyer, &policy(), demand(100_000), Millis(1_500)).expect("should renew");
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(
        p.first.cumulative,
        buyer.cumulative() + p.grant,
        "monotonic on this channel"
    );
}

#[test]
fn a_large_demand_spike_is_worth_the_forfeit() {
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.rate(), 1_250_000);

    // Demand jumps 16×. The remainder burns, but the design's own arithmetic
    // says a large jump costs only a few percent of the new grant.
    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(500)).expect("should buy");
    assert_eq!(p.trigger, Trigger::DemandRose);
    assert_eq!(p.rate, 20_000_000);
    assert!(p.forfeited > 0, "the remainder is given up");
    assert!(
        p.forfeited * 20 < p.grant,
        "forfeit {} should be small against the new grant {}",
        p.forfeited,
        p.grant
    );
}

#[test]
fn a_small_demand_rise_is_not() {
    // This is the hysteresis. A 20% rise mid-window would forfeit most of the
    // grant in force to buy barely more than it — the punitive case in the
    // design's forfeiture table.
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    assert!(
        poll(&buyer, &policy(), demand(1_200_000), Millis(500)).is_none(),
        "a 20% rise should not trigger an early re-buy"
    );
}

#[test]
fn falling_demand_never_triggers_an_early_buy() {
    // Buying cheaper mid-window still forfeits the expensive grant's remainder.
    // Letting it run out and renewing lower is strictly better.
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(10_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    assert!(poll(&buyer, &policy(), demand(1_000), Millis(500)).is_none());

    // ...but the renewal at the deadline does drop the rate.
    let p = poll(&buyer, &policy(), demand(1_000), Millis(1_500)).expect("should renew");
    assert_eq!(p.rate, 1_250);
}

#[test]
fn cumulative_only_ever_increases_on_a_given_channel() {
    // The Spilman ratchet requires it, and it is what makes TopUp idempotent.
    let mut buyer = opened(CAPACITY);
    let mut last = 0;

    for (step, rate) in [500_000u64, 8_000_000, 100_000, 40_000_000, 1_000]
        .into_iter()
        .enumerate()
    {
        let now = Millis(step as u64 * 2_000);
        let p = poll(&buyer, &policy(), demand(rate), now).expect("should buy");
        assert!(
            p.first.cumulative > last,
            "cumulative went backwards at step {step}"
        );
        last = p.first.cumulative;
        buyer.record(p, now);
    }
    assert_eq!(buyer.cumulative(), last);
}

#[test]
fn the_window_is_clamped_to_what_the_provider_advertised() {
    let buyer = opened(CAPACITY);

    // A provider on constrained hardware raising its floor.
    let constrained = Demand {
        observed_rate: 100_000,
        bounds: WindowBounds {
            min_ms: 5_000,
            max_ms: 30_000,
        },
    };
    let p = poll(&buyer, &policy(), constrained, Millis(0)).expect("should buy");
    assert_eq!(p.window_ms, 5_000, "raised to the provider's minimum");

    // A provider that will not sell far ahead.
    let short = Demand {
        observed_rate: 100_000,
        bounds: WindowBounds {
            min_ms: 200,
            max_ms: 1_000,
        },
    };
    let p = poll(&buyer, &policy(), short, Millis(0)).expect("should buy");
    assert_eq!(p.window_ms, 1_000, "lowered to the provider's maximum");
}

#[test]
fn a_rejection_is_answered_at_the_rate_the_provider_named() {
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    // We ask for 20 M/s; the provider says it can only do 4 M/s. Our own
    // ratchet rewinds, because the provider never turned its own.
    let asked = poll(&buyer, &policy(), demand(16_000_000), Millis(400)).expect("should buy");
    let before = buyer.cumulative();
    buyer.record(asked, Millis(400));
    buyer.record_reject(
        &[(asked.first.channel_id, asked.first.cumulative)],
        4_000_000,
        Millis(400),
        policy().cap_hold_ms,
    );
    assert_eq!(
        buyer.cumulative(),
        before,
        "the refused purchase was undone"
    );

    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(500)).expect("should re-buy");
    assert_eq!(p.trigger, Trigger::Rebuy);
    assert_eq!(p.rate, 4_000_000, "capped at what the provider will take");

    // The cap is held rather than forgotten: a purchase under it is not
    // evidence that it lifted, and re-probing every window would have the
    // provider refusing us once per window forever.
    buyer.record(p, Millis(500));
    assert!(
        poll(&buyer, &policy(), demand(16_000_000), Millis(600)).is_none(),
        "should not immediately ask above a cap it was just given"
    );

    // Once it expires, capacity may have freed up, so try again.
    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(11_000)).expect("should re-probe");
    assert_eq!(p.rate, 20_000_000);
}

#[test]
fn a_stale_rejection_does_not_rewind_anything() {
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    let committed = buyer.cumulative();

    buyer.record_reject(
        &[(channel(1), 42)],
        4_000_000,
        Millis(0),
        policy().cap_hold_ms,
    );
    assert_eq!(buyer.cumulative(), committed, "not a total we ever sent");
}

#[test]
fn the_operators_ceiling_is_respected() {
    // Vouchers cost money to acquire, whatever the protocol thinks.
    let policy = BuyerPolicy {
        max_rate: 5_000_000,
        ..policy()
    };
    let buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy, demand(100_000_000), Millis(0)).expect("should buy");
    assert_eq!(p.rate, 5_000_000);
}

#[test]
fn the_operators_floor_keeps_an_idle_link_ready() {
    let policy = BuyerPolicy {
        min_rate: 10_000,
        ..policy()
    };
    let buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy, demand(0), Millis(0)).expect("should buy");
    assert_eq!(p.rate, 10_000);
}

#[test]
fn an_idle_link_with_no_floor_buys_nothing() {
    // Otherwise a silent link would emit a TopUp every renewal interval forever.
    let buyer = opened(CAPACITY);
    assert!(poll(&buyer, &policy(), demand(0), Millis(0)).is_none());
}

#[test]
fn the_headroom_computation_saturates() {
    let buyer = opened(u64::MAX);
    let p = poll(&buyer, &policy(), demand(u64::MAX), Millis(0)).expect("should buy");
    assert_eq!(p.rate, u64::MAX);
}

// ---------------------------------------------------------------------------
// Rollover
// ---------------------------------------------------------------------------

#[test]
fn a_replacement_is_opened_at_the_threshold_and_only_once() {
    // Funding a channel on every check would open one per tick.
    let mut buyer = opened(1_000_000);
    assert!(!buyer.needs_rollover(80));

    // Drive it just past 80% of capacity.
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.cumulative(), 800_000);
    assert!(buyer.needs_rollover(80), "80% reached");

    // Once funding is under way it must stay quiet until the peer confirms.
    buyer.funded(channel(2), 1_000_000);
    assert!(!buyer.needs_rollover(80), "one is already on the way");

    buyer.confirmed(channel(2));
    assert!(
        !buyer.needs_rollover(80),
        "a replacement is ready and waiting"
    );
}

#[test]
fn a_confirmed_replacement_waits_behind_the_channel_in_use() {
    // It does not displace the one being drained — that one runs to its
    // capacity first, which is what keeps the old channel's remaining capacity
    // from being thrown away.
    let mut buyer = opened(1_000_000);
    buyer.funded(channel(2), 1_000_000);
    buyer.confirmed(channel(2));

    assert_eq!(buyer.active().expect("active").id, channel(1));
    assert_eq!(buyer.next_channel().expect("next").id, channel(2));
}

#[test]
fn a_purchase_that_overflows_is_signed_across_both_channels() {
    // The design's worked example: the old channel exhausts and the remainder
    // is signed onto the new one.
    let mut buyer = opened(1_000_000);
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.cumulative(), 800_000, "200 k of headroom left");

    buyer.funded(channel(2), 1_000_000);
    buyer.confirmed(channel(2));

    // The next 800 k grant is more than the 200 k the channel in use can carry.
    let p = poll(&buyer, &policy(), demand(320_000), Millis(1_600)).expect("should buy");
    assert_eq!(p.grant, 800_000);

    let first = p.first;
    let second = p.second.expect("the overflow needs its own channel");
    assert_eq!(first.channel_id, channel(1));
    assert_eq!(
        first.cumulative, 1_000_000,
        "topped to exactly its capacity"
    );
    assert_eq!(second.channel_id, channel(2));
    assert_eq!(
        second.cumulative, 600_000,
        "the remainder starts the new one"
    );
    assert_eq!(first.cumulative - 800_000 + second.cumulative, p.grant);
}

#[test]
fn an_exhausted_channel_is_retired_and_offered_for_settlement() {
    let mut buyer = opened(1_000_000);
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    assert_eq!(buyer.record(p, Millis(0)), None, "nothing retired yet");

    buyer.funded(channel(2), 1_000_000);
    buyer.confirmed(channel(2));

    let p = poll(&buyer, &policy(), demand(320_000), Millis(1_600)).expect("should buy");
    let retired = buyer.record(p, Millis(1_600));

    assert_eq!(
        retired,
        Some(channel(1)),
        "the drained channel can be settled"
    );
    assert_eq!(buyer.active().expect("active").id, channel(2));
    assert_eq!(buyer.next_channel(), None);
    assert_eq!(
        buyer.cumulative(),
        600_000,
        "the overflow carried onto the new channel"
    );
}

#[test]
fn without_a_replacement_the_buyer_takes_what_fits_and_then_stops() {
    // The channel really is full. Stopping is correct; what would be wrong is
    // signing a total the channel cannot settle.
    let mut buyer = opened(1_000_000);
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    let p = poll(&buyer, &policy(), demand(320_000), Millis(1_600)).expect("should buy");
    assert_eq!(p.grant, 200_000, "only what the channel could still carry");
    assert_eq!(p.first.cumulative, 1_000_000);
    assert_eq!(p.second, None);
    buyer.record(p, Millis(1_600));

    assert!(
        poll(&buyer, &policy(), demand(320_000), Millis(3_200)).is_none(),
        "nothing left to sign against and nowhere to move to"
    );
    assert!(
        buyer.needs_rollover(80),
        "and it is asking for a replacement"
    );
}

#[test]
fn cumulative_restarts_from_zero_on_the_replacement() {
    // A cumulative total only means anything against the channel it was signed
    // on, so carrying the old one forward would be meaningless — and would read
    // as a channel already near its capacity.
    let mut buyer = opened(1_000_000);
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    buyer.funded(channel(2), 1_000_000);
    buyer.confirmed(channel(2));
    assert_eq!(
        buyer.next_channel().expect("next").cumulative,
        0,
        "the replacement starts empty"
    );
}

#[test]
fn a_replacement_is_opened_before_the_channel_is_too_small_for_another_purchase() {
    // A threshold alone is reactive, and a purchase is not gradual: a grant can
    // take a channel from empty to nearly full in one step. Waiting for 80%
    // *signed* would leave nothing to move onto, and the peer would fall to the
    // minimum flow allowance while a replacement is funded and verified.
    let mut buyer = opened(3_000_000);

    // One purchase takes a third of the channel.
    let p = poll(&buyer, &policy(), demand(400_000), Millis(0)).expect("should buy");
    assert_eq!(p.grant, 1_000_000);
    buyer.record(p, Millis(0));

    // Only 2 M left, which is two more purchases — already worth replacing,
    // well before the 80% the threshold would wait for.
    assert_eq!(buyer.cumulative(), 1_000_000, "a third of the way through");
    assert!(
        buyer.needs_rollover(80),
        "should ask for a replacement while there is still room to use it"
    );
}

#[test]
fn a_channel_with_plenty_of_room_is_left_alone() {
    let mut buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy(), demand(400_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    assert!(
        !buyer.needs_rollover(80),
        "1 M against a 1 GiB channel is no reason to fund another"
    );
}

/// A provider that caps the window shorter than the configured lead must not
/// put the buyer in a renewal loop.
///
/// Taken literally, a lead longer than the window means every grant begins
/// already inside its own renewal lead: buy, renew, buy, renew, forfeiting all
/// of each one. The lead is bounded by the window actually in force instead.
#[test]
fn a_lead_longer_than_the_window_does_not_renew_forever() {
    let policy = BuyerPolicy {
        renew_lead_ms: 5_000,
        window_ms: 30_000,
        ..policy()
    };
    // The provider takes nothing longer than a second, so that is the window.
    let tight = Demand {
        observed_rate: 400_000,
        bounds: WindowBounds {
            min_ms: 200,
            max_ms: 1_000,
        },
    };

    let mut buyer = opened(CAPACITY);
    let first = poll(&buyer, &policy, tight, Millis(0)).expect("should buy");
    buyer.record(first, Millis(0));
    assert_eq!(buyer.deadline, Millis(1_000));

    // Half the window is the ceiling, so nothing is renewed in the first half.
    assert_eq!(policy.lead_within(1_000), 500);
    assert!(
        poll(&buyer, &policy, tight, Millis(1)).is_none(),
        "a grant one millisecond old is not due for renewal"
    );
    assert!(
        poll(&buyer, &policy, tight, Millis(400)).is_none(),
        "still not due at 40% of the window"
    );
    assert!(
        poll(&buyer, &policy, tight, Millis(500)).is_some(),
        "due once the remainder is down to the clamped lead"
    );
}

#[test]
fn the_forfeit_is_the_lead_over_the_window() {
    // What the default pair costs: 1.2 s of tolerance for 30% of each grant.
    let d = BuyerPolicy::default();
    assert_eq!(d.forfeit_pct(d.window_ms), 30);
    assert!(!d.lead_is_thin(), "the default must not be a trap");

    // Scaling both together buys absolute slack at the same proportional cost,
    // which is the whole reason the two are configured as a pair.
    let wider = BuyerPolicy {
        renew_lead_ms: 2_400,
        window_ms: 8_000,
        ..d
    };
    assert_eq!(wider.forfeit_pct(wider.window_ms), 30);

    // And a lead short enough to be missed under load is called out as one.
    let tight = BuyerPolicy {
        renew_lead_ms: 300,
        window_ms: 1_000,
        ..d
    };
    assert!(tight.lead_is_thin());
}
