//! The worked examples from `docs/design/core/tollgate-vouchers.md` and
//! `tollgate-protocol.md`, run against the implementation.
//!
//! These are the numbers the design argues from, so if any of them moves, the
//! documented economics no longer describe what the code does.

use alloc::vec;

use tollgate_protocol::{ChannelId, ChannelUpdate, ReasonCode, Signature};

use super::*;
use crate::config::GrantPolicy;
use crate::meter::Counters;
use crate::time::Millis;

/// Big enough that no test trips the channel ceiling by accident — the tests
/// that mean to check it set their own.
const ROOMY: u64 = 1_000_000_000_000;

fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

fn policy() -> GrantPolicy {
    GrantPolicy::default()
}

fn admission(policy: &GrantPolicy) -> Admission<'_> {
    Admission {
        policy,
        committed_elsewhere: 0,
    }
}

/// A provider that recognises one channel from this peer.
fn opened(capacity: u64) -> GrantState {
    let mut state = GrantState::new();
    state.open_channel(channel(1), capacity);
    state
}

/// The signature is checked by the host before core ever sees the message, so
/// its contents are irrelevant here.
fn update(ch: u8, cumulative: u64) -> ChannelUpdate {
    ChannelUpdate {
        channel_id: channel(ch),
        cumulative,
        signature: Signature([0; 64]),
    }
}

/// Evaluate and apply, the way the session does. Panics if refused, so a test
/// that expects a refusal calls [`evaluate_topup`] directly.
fn buy(state: &mut GrantState, updates: &[ChannelUpdate], window_ms: u32, now: Millis) -> u64 {
    let policy = policy();
    match evaluate_topup(state, admission(&policy), updates, window_ms) {
        Verdict::Accept {
            ratchets, grant, ..
        } => {
            state.apply(&ratchets, grant, window_ms, now);
            grant
        }
        Verdict::Reject { reason, .. } => panic!("refused: {reason:?}"),
    }
}

// ---------------------------------------------------------------------------
// rate = grant / window
// ---------------------------------------------------------------------------

#[test]
fn the_documented_rate_examples_hold() {
    // "5,120 bytes over 5 s → 1 KiB/s"
    assert_eq!(rate_from(5_120, 5_000), 1_024);
    // "512,000 bytes over 5 s → 100 KiB/s"
    assert_eq!(rate_from(512_000, 5_000), 102_400);
    // "512,000 bytes over 1 s → 500 KiB/s" — same proofs, shorter window
    assert_eq!(rate_from(512_000, 1_000), 512_000);
}

#[test]
fn rate_and_grant_are_inverses() {
    for &(rate, window) in &[(1_024u64, 5_000u32), (102_400, 5_000), (20_000_000, 250)] {
        assert_eq!(rate_from(grant_for(rate, window), window), rate);
    }
}

#[test]
fn rate_arithmetic_saturates_rather_than_wrapping() {
    // A peer picks both the grant and the window, so both are hostile inputs.
    assert_eq!(rate_from(u64::MAX, 1), u64::MAX);
    assert_eq!(units_in(u64::MAX, u64::MAX), u64::MAX);
    // A zero window is an unbounded rate; policy rejects it before this point.
    assert_eq!(rate_from(1, 0), u64::MAX);
}

// ---------------------------------------------------------------------------
// Grants replace each other
// ---------------------------------------------------------------------------

#[test]
fn raising_the_rate_mid_window_forfeits_the_remainder() {
    // The worked example: buy 6.25 M over 5 s, spend nothing, then at t=3
    // buy 100 M over 5 s. The 2.5 M left from the first grant burn at t=3.
    let mut state = opened(ROOMY);

    let grant = buy(&mut state, &[update(1, 6_250_000)], 5_000, Millis(0));
    assert_eq!(grant, 6_250_000);
    assert_eq!(state.rate(), 1_250_000, "1.25 M/s");
    assert_eq!(state.deadline(), Millis(5_000));

    // Three seconds of it drawn at the bought rate leaves 2.5 M.
    state.draw(3_750_000);
    assert_eq!(state.remaining(), 2_500_000);

    let grant = buy(&mut state, &[update(1, 106_250_000)], 5_000, Millis(3_000));
    assert_eq!(grant, 100_000_000, "this purchase alone");
    assert_eq!(state.rate(), 20_000_000, "20 M/s");
    assert_eq!(state.deadline(), Millis(8_000));
    assert_eq!(
        state.remaining(),
        100_000_000,
        "the 2.5 M remainder burned; only the new grant is spendable"
    );
}

#[test]
fn the_forfeiture_table_holds() {
    // "1.25 M/s, 2 s left, jump to 20 M/s → forfeit 2.5 M, 2.5% of the new grant"
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 5_000, Millis(0));
    state.draw(3_750_000);
    let forfeit = state.remaining();
    let new_grant = grant_for(20_000_000, 5_000);
    assert_eq!(forfeit, 2_500_000);
    assert_eq!(new_grant, 100_000_000);
    assert_eq!(forfeit * 1_000 / new_grant, 25, "2.5%");

    // "20 M/s, 4 s left, jump to 24 M/s → forfeit 80 M, 67% of the new grant"
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 100_000_000)], 5_000, Millis(0));
    state.draw(20_000_000);
    let forfeit = state.remaining();
    let new_grant = grant_for(24_000_000, 5_000);
    assert_eq!(forfeit, 80_000_000);
    assert_eq!(new_grant, 120_000_000);
    assert_eq!(forfeit * 100 / new_grant, 66, "≈67%");
}

#[test]
fn capacity_left_unused_is_not_banked() {
    // A buyer that waited must not be owed a burst just before the deadline:
    // the rate is fixed at grant/window regardless of what has been drawn.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 100_000)], 1_000, Millis(0));
    assert_eq!(state.rate(), 100_000);
    state.draw(0);
    assert_eq!(state.shaping_rate(Millis(900), 0), 100_000, "still 100 k/s");
}

#[test]
fn the_grant_expires_at_its_deadline_and_the_payment_is_kept() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 100_000)], 1_000, Millis(0));
    state.draw(40_000);

    assert!(!state.expire_if_due(Millis(999)), "not yet due");
    assert!(state.expire_if_due(Millis(1_000)), "60 k forfeited");
    assert_eq!(state.remaining(), 0);
    assert_eq!(state.authorized(), 100_000, "the payment is kept");
    assert!(
        !state.expire_if_due(Millis(2_000)),
        "nothing left to forfeit"
    );
}

#[test]
fn consumed_never_passes_authorized() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 1_000, Millis(0));
    state.draw(u64::MAX);
    assert_eq!(state.consumed(), 1_000);
    assert_eq!(state.remaining(), 0);
}

// ---------------------------------------------------------------------------
// A purchase spanning several channels
// ---------------------------------------------------------------------------

#[test]
fn the_grant_is_the_combined_increase_across_every_channel() {
    // The design's rollover example: the channel in use is topped to its
    // capacity and the remainder starts the replacement, in one purchase.
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000);
    state.open_channel(channel(2), 1_000_000);

    buy(&mut state, &[update(1, 800_000)], 2_000, Millis(0));
    assert_eq!(state.authorized(), 800_000);

    let grant = buy(
        &mut state,
        &[update(1, 1_000_000), update(2, 600_000)],
        2_000,
        Millis(2_000),
    );
    assert_eq!(
        grant, 800_000,
        "200 k from the old channel, 600 k from the new"
    );
    assert_eq!(
        state.rate(),
        400_000,
        "one rate, from the combined increase"
    );
    assert_eq!(state.authorized(), 1_600_000);
}

#[test]
fn a_purchase_is_refused_in_full_if_any_update_is_bad() {
    // Applying some of them would leave the grant a different size from the one
    // the payer asked for and thought it was paying for.
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000);
    state.open_channel(channel(2), 1_000_000);
    buy(&mut state, &[update(1, 500_000)], 2_000, Millis(0));

    let policy = policy();
    let verdict = evaluate_topup(
        &state,
        admission(&policy),
        // The second does not increase.
        &[update(1, 600_000), update(2, 0)],
        2_000,
    );
    assert!(matches!(
        verdict,
        Verdict::Reject {
            reason: ReasonCode::GrantInvalid,
            ..
        }
    ));
    assert_eq!(
        state.channel(channel(1)).expect("channel").signed,
        500_000,
        "the good update was not applied either"
    );
}

#[test]
fn the_same_channel_twice_in_one_purchase_is_refused() {
    // The second reading of `signed` would be stale and its delta counted from
    // the wrong base, inflating the grant.
    let state = opened(1_000_000);
    let policy = policy();
    assert!(matches!(
        evaluate_topup(
            &state,
            admission(&policy),
            &[update(1, 100_000), update(1, 200_000)],
            2_000
        ),
        Verdict::Reject {
            reason: ReasonCode::GrantInvalid,
            ..
        }
    ));
}

#[test]
fn an_update_on_an_unrecognised_channel_is_refused() {
    // Either never funded, or already settled. Refusing is what stops the
    // recognised set growing with every rollover a long session performs.
    let state = opened(1_000_000);
    let policy = policy();
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(9, 100_000)], 2_000),
        Verdict::Reject {
            reason: ReasonCode::FundingInvalid,
            ..
        }
    ));
}

#[test]
fn a_settled_channel_stops_being_recognised() {
    let mut state = opened(1_000_000);
    buy(&mut state, &[update(1, 500_000)], 2_000, Millis(0));
    state.close_channel(channel(1));

    let policy = policy();
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 600_000)], 2_000),
        Verdict::Reject {
            reason: ReasonCode::FundingInvalid,
            ..
        }
    ));
}

#[test]
fn a_channel_drained_to_capacity_is_reported_for_settlement() {
    let mut state = opened(1_000_000);
    assert_eq!(state.exhausted_channels().count(), 0);

    buy(&mut state, &[update(1, 1_000_000)], 2_000, Millis(0));
    assert_eq!(
        state.exhausted_channels().collect::<vec::Vec<_>>(),
        vec![channel(1)]
    );
}

// ---------------------------------------------------------------------------
// Shaping and the minimum flow allowance
// ---------------------------------------------------------------------------

#[test]
fn an_expired_grant_falls_back_to_the_allowance_not_to_silence() {
    // This is what keeps a link alive between grants: without it the peer could
    // never send the TopUp that revives it.
    const ALLOWANCE: u64 = 4_096;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 100_000)], 1_000, Millis(0));

    assert_eq!(state.shaping_rate(Millis(500), ALLOWANCE), 100_000);
    assert_eq!(state.shaping_rate(Millis(1_500), ALLOWANCE), ALLOWANCE);
}

#[test]
fn a_peer_that_has_never_paid_still_gets_the_allowance() {
    let state = GrantState::new();
    assert_eq!(state.shaping_rate(Millis(0), 4_096), 4_096);
    assert_eq!(
        state.shaping_rate(Millis(0), 0),
        0,
        "allowance off by default"
    );
}

#[test]
fn the_allowance_is_a_floor_not_an_addition() {
    // It is the floor of the shaper rather than a separate budget, so a peer
    // paying for more than the allowance does not also get the allowance.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 100_000)], 1_000, Millis(0));
    assert_eq!(state.shaping_rate(Millis(0), 4_096), 100_000);
}

// ---------------------------------------------------------------------------
// The received multiplier
// ---------------------------------------------------------------------------

#[test]
fn the_multiplier_table_holds() {
    // Per unit the peer uploads, its own grant is drawn by `m`, while we pay it
    // 1× for the delivery — so the net rate it pays is `m - 1`.
    let upload = Counters {
        delivered: 0,
        received: 1_000,
    };
    for &(m, drawn) in &[
        (0u16, 0u64),
        (1, 1_000),
        (2, 2_000),
        (10, 10_000),
        (11, 11_000),
    ] {
        assert_eq!(upload.weighted(m), drawn, "m = {m}");
    }
}

#[test]
fn a_download_always_draws_exactly_one_per_unit() {
    let download = Counters {
        delivered: 1_000,
        received: 0,
    };
    for m in [0u16, 1, 2, 11] {
        assert_eq!(
            download.weighted(m),
            1_000,
            "the multiplier is not on downloads"
        );
    }
}

#[test]
fn the_weighted_draw_down_saturates() {
    let hostile = Counters {
        delivered: u64::MAX,
        received: u64::MAX,
    };
    assert_eq!(
        hostile.weighted(u16::MAX),
        u64::MAX,
        "no wrap into free capacity"
    );
}

#[test]
fn a_counter_that_resets_reports_no_growth_rather_than_underflowing() {
    let earlier = Counters {
        delivered: 5_000,
        received: 5_000,
    };
    let after_reset = Counters::ZERO;
    assert_eq!(after_reset.delta_since(earlier), Counters::ZERO);
}

// ---------------------------------------------------------------------------
// Admission control
// ---------------------------------------------------------------------------

#[test]
fn a_cumulative_that_does_not_increase_is_refused() {
    // Replays and reorders are what make fire-and-forget purchases safe.
    let policy = policy();
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 10_000)], 1_000, Millis(0));

    for cumulative in [0, 5_000, 10_000] {
        assert!(
            matches!(
                evaluate_topup(&state, admission(&policy), &[update(1, cumulative)], 1_000),
                Verdict::Reject {
                    reason: ReasonCode::GrantInvalid,
                    ..
                }
            ),
            "cumulative {cumulative} should not ratchet"
        );
    }

    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 10_001)], 1_000),
        Verdict::Accept { grant: 1, .. }
    ));
}

#[test]
fn a_window_outside_the_advertised_range_is_refused() {
    let policy = policy();
    let state = opened(ROOMY);

    for window in [0, 1, policy.min_window_ms - 1, policy.max_window_ms + 1] {
        assert!(
            matches!(
                evaluate_topup(&state, admission(&policy), &[update(1, 1_000)], window),
                Verdict::Reject {
                    reason: ReasonCode::WindowOutOfRange,
                    ..
                }
            ),
            "window {window} ms should be refused"
        );
    }

    for window in [policy.min_window_ms, 5_000, policy.max_window_ms] {
        assert!(
            matches!(
                evaluate_topup(&state, admission(&policy), &[update(1, 1_000)], window),
                Verdict::Accept { .. }
            ),
            "window {window} ms should be accepted"
        );
    }
}

#[test]
fn a_rate_beyond_remaining_capacity_is_refused_with_the_rate_we_would_take() {
    // The point of refusing before taking the money: the payer re-purchases at
    // a rate that will be honored instead of inferring a shortfall later.
    let policy = GrantPolicy {
        max_rate: Some(10_000_000),
        ..GrantPolicy::default()
    };
    let state = opened(ROOMY);
    let admission = Admission {
        policy: &policy,
        committed_elsewhere: 6_000_000,
    };

    // 20 M/s asked for, 4 M/s left to give.
    assert_eq!(
        evaluate_topup(
            &state,
            admission,
            &[update(1, grant_for(20_000_000, 1_000))],
            1_000
        ),
        Verdict::Reject {
            reason: ReasonCode::RateExceedsCapacity,
            max_rate_available: 4_000_000,
        }
    );

    // Exactly the available rate fits.
    assert!(matches!(
        evaluate_topup(
            &state,
            admission,
            &[update(1, grant_for(4_000_000, 1_000))],
            1_000
        ),
        Verdict::Accept {
            rate: 4_000_000,
            ..
        }
    ));
}

#[test]
fn a_grant_beyond_the_channel_is_refused() {
    let policy = policy();
    let state = opened(50_000);

    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 50_001)], 1_000),
        Verdict::Reject {
            reason: ReasonCode::GrantExceedsChannel,
            ..
        }
    ));
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 50_000)], 1_000),
        Verdict::Accept { .. }
    ));
}

#[test]
fn an_unset_rate_ceiling_means_the_link_is_the_limit() {
    let policy = policy();
    assert_eq!(policy.max_rate, None);
    let admission = Admission {
        policy: &policy,
        committed_elsewhere: u64::MAX,
    };
    assert_eq!(admission.rate_available(), u64::MAX);
}
