#![cfg(feature = "spilman")]
//! Does `TollGateHost` behave the way `SpilmanBridge` expects it to?
//!
//! The unit tests in `tollgate_net::spilman::host` check each host hook on its
//! own. This file checks the seam: it drives the real `cdk-spilman`
//! [`SpilmanBridge`] with a TollGate host and asserts which hook each gate
//! reached — channel state, receiver key, mint/keyset cache, unit policy,
//! capacity, expiry. Everything here is offline and deterministic; the parts
//! that need a live mint (funding swap, signature verification, settlement)
//! belong to the mint-networking and driver-wiring milestones.

use cashu::nuts::{CurrencyUnit, Id, SecretKey};
use cdk_spilman::{
    BridgeError, ChannelFunding, ChannelPolicy, PaymentProof, SpilmanBridge, SpilmanHost,
};
use serde_json::json;
use tollgate_core::pricing::Price;
use tollgate_net::spilman::TollGateHost;

/// Fixed, valid secp256k1 scalars (test-only).
const NODE_SECRET_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SENDER_SECRET_HEX: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const MINT: &str = "https://mint.test";
const OTHER_MINT: &str = "https://other.mint.test";
const KEYSET_ID: &str = "009a1f293253e41e";
const CHANNEL: &str = "channel-1";
const MAX_AMOUNT_PER_OUTPUT: u64 = 64;

fn node_secret() -> SecretKey {
    SecretKey::from_hex(NODE_SECRET_HEX).expect("valid node secret")
}

fn node_pubkey_hex() -> String {
    node_secret().public_key().to_hex()
}

fn sender_pubkey_hex() -> String {
    SecretKey::from_hex(SENDER_SECRET_HEX)
        .expect("valid sender secret")
        .public_key()
        .to_hex()
}

fn keyset_id() -> Id {
    KEYSET_ID.parse().expect("valid v1 keyset id")
}

fn tight_policy() -> ChannelPolicy {
    ChannelPolicy {
        min_capacity: 1_000,
        min_expiry_in_seconds: 3_600,
        max_amount_per_output: Some(MAX_AMOUNT_PER_OUTPUT),
    }
}

/// A payment message the way a buyer sends it: cumulative balance plus the
/// channel params the server needs to validate a channel it has never seen.
fn payment_json(mint: &str, capacity: u64, expiry_timestamp: u64, now_seconds: u64) -> String {
    json!({
        "channel_id": CHANNEL,
        "balance": 0,
        "signature": "aa".repeat(64),
        "params": {
            "sender_pubkey": sender_pubkey_hex(),
            "receiver_pubkey": node_pubkey_hex(),
            "mint": mint,
            "unit": "sat",
            "capacity": capacity,
            "funding_token_amount": capacity,
            "maximum_amount": MAX_AMOUNT_PER_OUTPUT,
            "expiry_timestamp": expiry_timestamp,
            "setup_timestamp": now_seconds,
            "keyset_id": KEYSET_ID,
        },
        "funding_proofs": [],
    })
    .to_string()
}

/// A host that trusts `MINT` and has cached its keyset, so every gate up to the
/// funding-swap math can be walked deliberately.
fn ready_host() -> TollGateHost {
    let host = TollGateHost::new(node_secret())
        .with_accepted_mint(MINT)
        .with_default_price(Price {
            per_second: 1_000,
            per_unit: 0,
        })
        .with_default_policy(tight_policy());
    host.add_keyset(MINT, &CurrencyUnit::Sat, keyset_id(), "{}", true);
    host
}

fn now(host: &TollGateHost) -> u64 {
    host.now_seconds()
}

fn bridge_error(host: TollGateHost, payment: &str) -> BridgeError {
    SpilmanBridge::new(host)
        .process_payment_via_json(payment, &"{}".to_string())
        .expect_err("payment is rejected")
}

#[test]
fn receiver_key_gate_matches_the_host_policy() {
    let now_seconds = now(&ready_host());
    let far_future = 4_000_000_000;

    // Default policy accepts any receiver key, so the next gate is the mint.
    let err = bridge_error(
        ready_host(),
        &payment_json(OTHER_MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "mint or keyset not acceptable");

    // Pinned to a foreign key, the bridge refuses before it ever looks at the
    // mint — the honest answer for a node asked to settle somebody else's channel.
    let other_pubkey = SecretKey::from_hex(SENDER_SECRET_HEX)
        .expect("valid secret")
        .public_key();
    let pinned_elsewhere = ready_host().with_receiver_key(other_pubkey);
    let err = bridge_error(
        pinned_elsewhere,
        &payment_json(MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "receiver key not acceptable");

    // Pinned to our own key the same gate passes; the untrusted mint is what
    // stops the payment.
    let pinned_to_us = ready_host().with_receiver_key(node_secret().public_key());
    let err = bridge_error(
        pinned_to_us,
        &payment_json(OTHER_MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(
        err.to_string(),
        "mint or keyset not acceptable",
        "our own receiver key passes, so the mint is the gate that fires"
    );
}

#[test]
fn keyset_cache_policy_and_expiry_gates_are_reached_in_order() {
    let now_seconds = now(&ready_host());
    let far_future = 4_000_000_000;

    // Trusted mint, keyset never fetched: `mint_and_keyset_is_acceptable` lets it
    // through (the mint is the authority) but `get_keyset_info` has nothing to
    // hand the bridge, so funding stops here.
    let unfetched = TollGateHost::new(node_secret()).with_accepted_mint(MINT);
    let err = bridge_error(
        unfetched,
        &payment_json(MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "mint or keyset not acceptable");

    // Cached keyset, capacity below the pinned policy's minimum.
    let err = bridge_error(
        ready_host(),
        &payment_json(MINT, 10, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "capacity too small: 10 < 1000");

    // Capacity fine, expiry inside the minimum window.
    let err = bridge_error(
        ready_host(),
        &payment_json(MINT, 10_000, now_seconds + 60, now_seconds),
    );
    assert!(
        err.to_string().starts_with("expiry too soon"),
        "unexpected error: {err}"
    );

    // Unit allow-list: pinning one unit makes every other unit unsupported.
    let sat_only = ready_host().with_unit_policy("msat", tight_policy());
    let err = bridge_error(
        sat_only,
        &payment_json(MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "unsupported unit: sat");
}

#[test]
fn every_host_gate_can_be_passed_and_funding_stops_at_the_mint_math() {
    let now_seconds = now(&ready_host());
    // Capacity above the minimum, expiry beyond the policy window: every hook
    // this host implements is satisfied. The bridge then calls
    // `compute_channel_secret` (our ECDH) and moves on to parse the mint's
    // keyset info — which this offline test deliberately left empty, so that is
    // exactly where it stops.
    let far_future = now_seconds + tight_policy().min_expiry_in_seconds + 60;
    let err = bridge_error(
        ready_host(),
        &payment_json(MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(
        err.to_string(),
        "Missing or invalid 'keysetId' field",
        "expected every host gate to pass (and ECDH to succeed), got: {err}"
    );
}

#[test]
fn closing_and_closed_channels_are_refused_up_front() {
    // The bridge consults `get_channel_state` before anything else, so a channel
    // that has been settled can never be replayed for credit.
    let funding = ChannelFunding {
        params_json: "{}".to_string(),
        funding_proofs_json: "[]".to_string(),
        channel_secret_hex: "00".repeat(32),
        keyset_info_json: "{}".to_string(),
    };
    let payment = PaymentProof {
        balance: 5,
        signature: "bb".repeat(64),
    };
    let now_seconds = now(&ready_host());
    let far_future = 4_000_000_000;

    let closing = ready_host();
    closing.save_funding(CHANNEL, funding.clone(), payment.clone());
    closing
        .mark_channel_closing(CHANNEL, far_future, payment.clone())
        .expect("channel enters closing");
    let err = bridge_error(
        closing,
        &payment_json(MINT, 10_000, far_future, now_seconds),
    );
    assert_eq!(err.to_string(), "channel closing, swap pending");

    let closed = ready_host();
    closed.save_funding(CHANNEL, funding, payment);
    closed
        .mark_channel_closed(CHANNEL, far_future, 5, "[]", "[]", 5, 995)
        .expect("channel closes");
    let err = bridge_error(closed, &payment_json(MINT, 10_000, far_future, now_seconds));
    assert_eq!(err.to_string(), "channel closed");
}
