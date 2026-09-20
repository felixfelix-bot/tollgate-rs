//! [`SpilmanHost`] for a TollGate node — the server side of a Spilman channel.
//!
//! # Where this sits
//!
//! `cdk-spilman`'s `SpilmanBridge` drives a channel: it validates the funding
//! token, verifies the sender's BIP-340 Schnorr balance updates, and decides
//! whether a payment covers the usage. It knows nothing about TollGate, so it
//! asks this host for every deployment-specific fact through the 17
//! [`SpilmanHost`] hooks:
//!
//! | Hook(s) | TollGate answer |
//! |---|---|
//! | `receiver_key_is_acceptable` | Accept any (default), or pin this node's key |
//! | `mint_and_keyset_is_acceptable` | Mint must be in `accepted_mints`, keyset known-and-active |
//! | `get_funding` / `save_funding` | In-memory channel records |
//! | `get_amount_due` | `tollgate-core` pricing: rate × seconds (+ per unit) |
//! | `record_payment` | Store the signed balance and advance the usage meter |
//! | `get_channel_state`, `mark_channel_closing/closed`, `get_closing_data` | Channel state machine |
//! | `compute_channel_secret` | ECDH(`node_secret`, sender pubkey) |
//! | `sign_with_tweaked_key` | BIP-340 Schnorr over the P2BK-tweaked node key |
//! | `get_active_keyset_ids` / `get_keyset_info` | Keyset cache filled from the mint |
//! | `now_seconds` | `SystemTime` |
//!
//! # Storage: in-memory first
//!
//! Channel records live in a `HashMap` behind a `Mutex`. A restart loses them —
//! acceptable for the first cut (a peer simply re-funds) and the reason the
//! accessors below are shaped like a store: the upgrade path is a `rusqlite`
//! implementation behind the same method set, which is what `cdk-spilman`'s own
//! `ConfigurableHost` does with `SpilmanStorage`.
//!
//! # Metering → amount due
//!
//! `get_amount_due(cid, ctx)` is where TollGate metering meets a Spilman
//! balance. The node charges a [`Price`] in *scaled* integers
//! (`tollgate_core::pricing`, scale = `pricing_scale`, default 1000 = milli-sats)
//! and `tollgate-core`'s [`Price::cost_scaled`] does the arithmetic:
//!
//! ```text
//! cost_scaled = elapsed_ms / 1000 × per_second + units_delivered × per_unit
//! amount_due  = ceil(cost_scaled / pricing_scale)            // whole sats
//! ```
//!
//! so a channel's balance is denominated like a bootstrap token's credit
//! (`BootstrapWallet::verify` returns the same milli-unit scale). Usage for a
//! channel accrues from two places, mirroring `ConfigurableHost`'s accumulated +
//! pending model:
//!
//! * **accumulated** — what the node has already decided to bill, fed in by the
//!   metering loop ([`TollGateHost::record_usage`]) or by `record_payment`;
//! * **pending** — the per-request increments the bridge passes as its `context`
//!   argument: a JSON object `{"seconds": N, "units": M}` (both optional).
//!
//! Time is billed on the wall clock by default, measured from the channel's
//! baseline (the moment funding landed), which is the "rate × seconds" model the
//! TollGate design uses. A node that meters out of band — only billing seconds it
//! actually delivered, e.g. while a peer is suspended — turns that off with
//! [`TollGateHost::set_wall_clock_billing`] and feeds `record_usage` instead.
//!
//! A negative [`Price`] (the node pays the peer to attract traffic — see
//! `docs/design/core/tollgate-pricing.md`) clamps the amount due at zero:
//! a Spilman balance is unsigned, so there is nothing to bill.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use cashu::nuts::{CurrencyUnit, Id, PublicKey, SecretKey};
use cdk_spilman::{
    ChannelFunding, ChannelPolicy, ChannelState, ClosingData, PaymentProof, SpilmanHost,
    compute_channel_secret_from_hex, sign_with_tweaked_key_util,
};
use serde::Deserialize;
use tollgate_core::metering::Counters;
use tollgate_core::pricing::Price;
use tollgate_protocol::DEFAULT_PRICING_SCALE;

/// Lock helper: a poisoned mutex means some other thread panicked mid-update.
/// The maps below hold only plain data, so recovering the guard is safe and
/// strictly better than propagating the panic into the payment path.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Pending usage increments carried in the bridge's `context` argument.
///
/// `{"seconds": 1, "units": 1024}` bills one more second and 1024 more delivered
/// units on top of whatever the host has already accumulated. Unknown fields are
/// ignored; a missing or unparsable context means "no pending usage".
#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct PendingUsage {
    #[serde(default)]
    seconds: u64,
    #[serde(default)]
    units: u64,
}

impl PendingUsage {
    /// Parse a bridge context JSON string into pending increments.
    fn parse(context: Option<&String>) -> Self {
        context
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default()
    }

    /// `(elapsed_ms, units)` form used by the meter.
    fn as_totals(self) -> (u64, u64) {
        (self.seconds.saturating_mul(1_000), self.units)
    }
}

/// Per-channel usage meter: the price the peer agreed to plus everything the
/// node has decided to bill since the channel's baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelMeter {
    /// Rate charged on this channel, in scaled units (see `tollgate_core::Price`).
    price: Price,
    /// Divisor that converts scaled cost into whole sats (`cost = ceil(scaled / scale)`).
    pricing_scale: u32,
    /// Unix seconds the current billing baseline was taken (channel funding, or
    /// the last paid interval when wall-clock billing is on).
    baseline_secs: u64,
    /// Elapsed time already accounted for out of band, in milliseconds.
    metered_ms: u64,
    /// Delivered units already accounted for out of band.
    metered_units: u64,
    /// Whether time since the baseline is billed on the wall clock.
    bill_wall_clock: bool,
}

impl ChannelMeter {
    /// A meter that starts billing wall-clock time from `baseline_secs`.
    pub fn new(price: Price, pricing_scale: u32, baseline_secs: u64) -> Self {
        Self {
            price,
            pricing_scale: pricing_scale.max(1),
            baseline_secs,
            metered_ms: 0,
            metered_units: 0,
            bill_wall_clock: true,
        }
    }

    /// The rate charged on this channel.
    pub fn price(&self) -> Price {
        self.price
    }

    /// The pricing scale divisor.
    pub fn pricing_scale(&self) -> u32 {
        self.pricing_scale
    }

    /// Unix seconds the current billing baseline was taken.
    pub fn baseline_secs(&self) -> u64 {
        self.baseline_secs
    }

    /// Accumulate usage the node has decided to bill (the metering tick).
    pub fn add_usage(&mut self, elapsed_ms: u64, units: u64) {
        self.metered_ms = self.metered_ms.saturating_add(elapsed_ms);
        self.metered_units = self.metered_units.saturating_add(units);
    }

    /// Move the wall-clock baseline to `now_secs`. Called after a payment is
    /// accepted so the seconds just paid for are never billed twice.
    pub fn rebaseline(&mut self, now_secs: u64) {
        self.baseline_secs = now_secs;
    }

    /// Bill wall-clock time from the baseline (`true`, the default) or only
    /// usage fed in through [`Self::add_usage`] (`false`).
    pub fn set_wall_clock_billing(&mut self, on: bool) {
        self.bill_wall_clock = on;
    }

    /// Whether wall-clock time is currently billed.
    pub fn bills_wall_clock(&self) -> bool {
        self.bill_wall_clock
    }

    /// Whole sats owed at `now_secs`, given `pending` usage not yet accumulated.
    ///
    /// `ceil(cost_scaled / pricing_scale)`, clamped at zero for free or negative
    /// (we-pay-the-peer) pricing.
    pub fn amount_due(&self, now_secs: u64, pending: (u64, u64)) -> u64 {
        let (elapsed_ms, units) = self.totals(now_secs, pending);
        Self::ceil_scaled(
            self.price.cost_scaled(elapsed_ms, units),
            self.pricing_scale,
        )
    }

    /// `(elapsed_ms, units)` billable at `now_secs`.
    fn totals(&self, now_secs: u64, pending: (u64, u64)) -> (u64, u64) {
        let wall_ms = if self.bill_wall_clock {
            now_secs
                .saturating_sub(self.baseline_secs)
                .saturating_mul(1_000)
        } else {
            0
        };
        (
            self.metered_ms
                .saturating_add(wall_ms)
                .saturating_add(pending.0),
            self.metered_units.saturating_add(pending.1),
        )
    }

    /// Scale a signed cost down to whole sats, rounding up (never billing a
    /// fraction of a sat) and clamping negatives to zero.
    fn ceil_scaled(cost_scaled: i64, pricing_scale: u32) -> u64 {
        if cost_scaled <= 0 {
            return 0;
        }
        // `div_ceil` is only stable for unsigned integers, so clamp into u64
        // first (the cost is strictly positive here) and round up there.
        let scaled = u64::try_from(cost_scaled).unwrap_or(u64::MAX);
        scaled.div_ceil(u64::from(pricing_scale.max(1)))
    }
}

/// What a channel settled to, kept for audit and status reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSettlement {
    /// The channel's expiry timestamp.
    pub expiry_timestamp: u64,
    /// Final balance owed to this node, in sats.
    pub balance: u64,
    /// Sum of the receiver's (our) post-stage-2 proofs.
    pub receiver_sum: u64,
    /// Sum of the sender's change proofs.
    pub sender_sum: u64,
    /// Receiver proof bundle as handed to the host at close.
    pub receiver_proofs_json: String,
    /// Sender proof bundle as handed to the host at close.
    pub sender_proofs_json: String,
}

/// A cached keyset from a mint's `/v1/keysets`.
#[derive(Clone, Debug)]
struct KeysetEntry {
    mint: String,
    /// `CurrencyUnit` as a string, so comparisons don't depend on the enum's
    /// `Hash`/`Eq` impls.
    unit: String,
    /// The keyset info JSON as fetched, returned verbatim by `get_keyset_info`.
    info_json: String,
    active: bool,
}

/// Everything the node knows about one channel.
#[derive(Clone, Debug, Default)]
struct ChannelRecord {
    funding: Option<ChannelFunding>,
    /// Latest accepted balance update: the running balance plus the sender's
    /// signature over it. Doubles as the unilateral-exit proof.
    payment: Option<PaymentProof>,
    state: Option<ChannelState>,
    closing: Option<ClosingData>,
    settlement: Option<ChannelSettlement>,
    meter: Option<ChannelMeter>,
}

impl ChannelRecord {
    fn state(&self) -> ChannelState {
        // Matches `cdk-spilman`'s own storage: an unknown/unfunded channel is Open.
        self.state.unwrap_or(ChannelState::Open)
    }

    fn ensure_meter(
        &mut self,
        price: Price,
        pricing_scale: u32,
        now_secs: u64,
    ) -> &mut ChannelMeter {
        self.meter
            .get_or_insert_with(|| ChannelMeter::new(price, pricing_scale, now_secs))
    }
}

/// The TollGate node's [`SpilmanHost`]: accepts channel funding, meters usage
/// against `tollgate-core` pricing, and signs settlements with the node key.
///
/// Cheap to build; wrap in an `Arc` and hand it to `SpilmanBridge::new`.
#[derive(Debug)]
pub struct TollGateHost {
    node_secret: SecretKey,
    node_secret_hex: String,
    node_pubkey_hex: String,
    /// Mints whose channels we accept. Empty = trust nothing (fail closed).
    accepted_mints: HashSet<String>,
    /// Default pricing scale; 1000 = milli-sat precision.
    pricing_scale: u32,
    /// Rate used when a channel is funded without a registered meter.
    default_price: Price,
    /// Policy applied to units without an explicit entry.
    default_policy: ChannelPolicy,
    /// Per-unit policy overrides. When non-empty, an unlisted unit is refused.
    unit_policies: HashMap<String, ChannelPolicy>,
    /// Receiver keys to accept. Empty = accept any (the funding token is only
    /// spendable by the key that can complete the swap, so a wrong key fails
    /// safely at settlement).
    accepted_receiver_keys: Vec<PublicKey>,
    channels: Mutex<HashMap<String, ChannelRecord>>,
    keysets: Mutex<HashMap<Id, KeysetEntry>>,
    /// `(mint, unit)` → active keyset ids, for `get_active_keyset_ids`.
    active_keysets: Mutex<HashMap<(String, String), Vec<Id>>>,
}

impl TollGateHost {
    /// A host signing as `node_secret`, accepting no mint until one is trusted.
    pub fn new(node_secret: SecretKey) -> Self {
        let node_pubkey_hex = node_secret.public_key().to_hex();
        let node_secret_hex = node_secret.to_secret_hex();
        Self {
            node_secret,
            node_secret_hex,
            node_pubkey_hex,
            accepted_mints: HashSet::new(),
            pricing_scale: DEFAULT_PRICING_SCALE,
            default_price: Price::default(),
            default_policy: default_channel_policy(),
            unit_policies: HashMap::new(),
            accepted_receiver_keys: Vec::new(),
            channels: Mutex::new(HashMap::new()),
            keysets: Mutex::new(HashMap::new()),
            active_keysets: Mutex::new(HashMap::new()),
        }
    }

    /// Trust a mint: channels funded from it become acceptable.
    #[must_use]
    pub fn with_accepted_mint(mut self, mint: impl Into<String>) -> Self {
        self.accepted_mints.insert(mint.into());
        self
    }

    /// Rate used for channels funded before a meter is registered.
    #[must_use]
    pub fn with_default_price(mut self, price: Price) -> Self {
        self.default_price = price;
        self
    }

    /// Set the pricing scale divisor (defaults to
    /// [`DEFAULT_PRICING_SCALE`](tollgate_protocol::DEFAULT_PRICING_SCALE)).
    #[must_use]
    pub fn with_pricing_scale(mut self, scale: u32) -> Self {
        self.pricing_scale = scale.max(1);
        self
    }

    /// Policy applied to every unit that has no explicit override.
    #[must_use]
    pub fn with_default_policy(mut self, policy: ChannelPolicy) -> Self {
        self.default_policy = policy;
        self
    }

    /// Pin a policy for one unit (`"sat"`, `"msat"`, …). Once any unit is
    /// pinned, units without an entry are refused rather than defaulted.
    #[must_use]
    pub fn with_unit_policy(mut self, unit: &str, policy: ChannelPolicy) -> Self {
        self.unit_policies.insert(unit.to_string(), policy);
        self
    }

    /// Accept only channels addressed to `receiver_key`. Without this the host
    /// accepts any receiver key, per the integration plan.
    #[must_use]
    pub fn with_receiver_key(mut self, receiver_key: PublicKey) -> Self {
        self.accepted_receiver_keys.push(receiver_key);
        self
    }

    /// This node's public key (the Spilman receiver key).
    pub fn node_pubkey(&self) -> PublicKey {
        self.node_secret.public_key()
    }

    /// This node's public key as hex — what peers must put in channel params.
    pub fn node_pubkey_hex(&self) -> &str {
        &self.node_pubkey_hex
    }

    /// The rate used for newly funded channels.
    pub fn default_price(&self) -> Price {
        self.default_price
    }

    /// The mints this host trusts.
    pub fn accepted_mints(&self) -> &HashSet<String> {
        &self.accepted_mints
    }

    // -- keyset cache ---------------------------------------------------------

    /// Cache a keyset fetched from `mint`'s `/v1/keysets`. `info_json` is the
    /// keyset-info JSON the mint returned, stored verbatim because
    /// `get_keyset_info` must hand exactly that back to `cdk-spilman`.
    pub fn add_keyset(
        &self,
        mint: &str,
        unit: &CurrencyUnit,
        keyset_id: Id,
        info_json: impl Into<String>,
        active: bool,
    ) {
        let unit = unit.to_string();
        lock(&self.keysets).insert(
            keyset_id,
            KeysetEntry {
                mint: mint.to_string(),
                unit: unit.clone(),
                info_json: info_json.into(),
                active,
            },
        );

        let mut active_keysets = lock(&self.active_keysets);
        let slot = active_keysets.entry((mint.to_string(), unit)).or_default();
        if active && !slot.contains(&keyset_id) {
            slot.push(keyset_id);
        } else if !active {
            slot.retain(|id| id != &keyset_id);
        }
    }

    /// Flip a cached keyset's active flag (mints rotate keysets).
    pub fn set_keyset_active(&self, keyset_id: &Id, active: bool) {
        let mut keysets = lock(&self.keysets);
        let Some(entry) = keysets.get_mut(keyset_id) else {
            return;
        };
        entry.active = active;
        let key = (entry.mint.clone(), entry.unit.clone());
        drop(keysets);

        let mut active_keysets = lock(&self.active_keysets);
        let slot = active_keysets.entry(key).or_default();
        if active {
            if !slot.contains(keyset_id) {
                slot.push(*keyset_id);
            }
        } else {
            slot.retain(|id| id != keyset_id);
        }
    }

    /// Number of keysets in the cache (status/monitoring).
    pub fn keyset_count(&self) -> usize {
        lock(&self.keysets).len()
    }

    // -- channel-side helpers used by the driver -----------------------------

    /// Register the rate for a channel and start (or restart) its billing
    /// baseline. Call this when a peer's Announce advertises `CAP_SPILMAN` with
    /// a negotiated price; funding alone uses [`Self::default_price`].
    pub fn register_channel_meter(&self, channel_id: &str, price: Price, now_secs: u64) {
        let mut channels = lock(&self.channels);
        let record = channels.entry(channel_id.to_string()).or_default();
        record.meter = Some(ChannelMeter::new(price, self.pricing_scale, now_secs));
    }

    /// Feed the metering loop's tick: `elapsed_ms` wall time plus `delivered`
    /// units to bill. With wall-clock billing on (the default) pass `elapsed_ms
    /// = 0` and let the baseline carry the time; with it off, this is the only
    /// source of time.
    pub fn record_usage(&self, channel_id: &str, elapsed_ms: u64, delivered: Counters) {
        let now = self.now_seconds();
        let mut channels = lock(&self.channels);
        let record = channels.entry(channel_id.to_string()).or_default();
        let meter = record.ensure_meter(self.default_price, self.pricing_scale, now);
        meter.add_usage(elapsed_ms, delivered.delivered);
    }

    /// Bill wall-clock time from the baseline (`on`) or only explicit usage.
    pub fn set_wall_clock_billing(&self, channel_id: &str, on: bool) {
        let now = self.now_seconds();
        let mut channels = lock(&self.channels);
        let record = channels.entry(channel_id.to_string()).or_default();
        let meter = record.ensure_meter(self.default_price, self.pricing_scale, now);
        // Moving to wall-clock billing starts from "now", never retroactively.
        if on && !meter.bills_wall_clock() {
            meter.rebaseline(now);
        }
        meter.set_wall_clock_billing(on);
    }

    /// Sats this node has been paid on the channel (last accepted balance).
    pub fn paid_balance(&self, channel_id: &str) -> u64 {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.payment.as_ref())
            .map_or(0, |payment| payment.balance)
    }

    /// Sats owed right now, ignoring any in-flight request context.
    pub fn current_amount_due(&self, channel_id: &str) -> u64 {
        self.get_amount_due(channel_id, None)
    }

    /// The access decision: does the channel's paid balance cover what is owed?
    pub fn is_covered(&self, channel_id: &str) -> bool {
        self.paid_balance(channel_id) >= self.current_amount_due(channel_id)
    }

    /// The channel's current state (`Open` when unknown).
    pub fn channel_state(&self, channel_id: &str) -> ChannelState {
        self.get_channel_state(channel_id)
    }

    /// Settlement record, once the channel has closed.
    pub fn settlement(&self, channel_id: &str) -> Option<ChannelSettlement> {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.settlement.clone())
    }

    /// Channel ids this host is tracking.
    pub fn channel_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.channels).keys().cloned().collect();
        ids.sort();
        ids
    }

    /// The usage meter for a channel, if one exists.
    pub fn channel_meter(&self, channel_id: &str) -> Option<ChannelMeter> {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.meter)
    }

    /// Reject a channel that is not addressed to this node's key. We cannot
    /// derive the ECDH channel secret or sign a settlement for someone else's
    /// channel, so failing loudly beats producing a token no mint will accept.
    fn require_our_key(&self, role: &str, pubkey_hex: &str) -> Result<(), String> {
        if pubkey_hex == self.node_pubkey_hex {
            Ok(())
        } else {
            Err(format!(
                "{role} pubkey {pubkey_hex} is not this node's key {}",
                self.node_pubkey_hex
            ))
        }
    }
}

/// A deliberately permissive policy for units without an override: channels
/// must be able to cover at least one sat and outlive an hour.
fn default_channel_policy() -> ChannelPolicy {
    ChannelPolicy {
        min_capacity: 1,
        min_expiry_in_seconds: 3_600,
        max_amount_per_output: None,
    }
}

impl SpilmanHost for TollGateHost {
    /// Accept any receiver key by default (the integration plan's behaviour), or
    /// the pinned keys when [`TollGateHost::with_receiver_key`] was used.
    fn receiver_key_is_acceptable(&self, receiver_pubkey: &PublicKey) -> bool {
        self.accepted_receiver_keys.is_empty()
            || self.accepted_receiver_keys.contains(receiver_pubkey)
    }

    /// Trust the mint, and trust the keyset as long as the cache does not say it
    /// has been rotated away. An unknown keyset is left to the mint to reject at
    /// swap time rather than failing the funding here.
    fn mint_and_keyset_is_acceptable(&self, mint: &str, keyset_id: &Id) -> bool {
        if !self.accepted_mints.contains(mint) {
            return false;
        }
        match lock(&self.keysets).get(keyset_id) {
            Some(entry) => entry.active && entry.mint == mint,
            None => true,
        }
    }

    fn get_funding(&self, channel_id: &str) -> Option<ChannelFunding> {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.funding.clone())
    }

    fn save_funding(
        &self,
        channel_id: &str,
        funding: ChannelFunding,
        initial_payment: PaymentProof,
    ) {
        let now = self.now_seconds();
        let mut channels = lock(&self.channels);
        let record = channels.entry(channel_id.to_string()).or_default();
        record.funding = Some(funding);
        record.payment = Some(initial_payment);
        record.state = Some(ChannelState::Open);
        // Funding is the metering baseline: the peer starts paying for time now.
        record
            .ensure_meter(self.default_price, self.pricing_scale, now)
            .rebaseline(now);
    }

    fn get_amount_due(&self, channel_id: &str, context: Option<&String>) -> u64 {
        let pending = PendingUsage::parse(context).as_totals();
        let now = self.now_seconds();
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.meter.as_ref())
            .map_or(0, |meter| meter.amount_due(now, pending))
    }

    /// The bridge calls this only once the balance covers the amount due, so the
    /// usage those sats bought is folded into the meter here and the wall-clock
    /// baseline moves up — the paid-for seconds are never billed again.
    fn record_payment(&self, channel_id: &str, payment: PaymentProof, context: &String) {
        let pending = PendingUsage::parse(Some(context)).as_totals();
        let now = self.now_seconds();
        let mut channels = lock(&self.channels);
        let record = channels.entry(channel_id.to_string()).or_default();
        record.payment = Some(payment);
        let meter = record.ensure_meter(self.default_price, self.pricing_scale, now);
        meter.add_usage(pending.0, pending.1);
        meter.rebaseline(now);
    }

    fn get_channel_state(&self, channel_id: &str) -> ChannelState {
        lock(&self.channels)
            .get(channel_id)
            .map_or(ChannelState::Open, ChannelRecord::state)
    }

    fn mark_channel_closing(
        &self,
        channel_id: &str,
        expiry_timestamp: u64,
        payment: PaymentProof,
    ) -> Result<(), String> {
        let mut channels = lock(&self.channels);
        let record = channels
            .get_mut(channel_id)
            .ok_or_else(|| format!("unknown channel {channel_id}"))?;
        if record.state() == ChannelState::Closed {
            return Err(format!("channel {channel_id} is already closed"));
        }
        record.state = Some(ChannelState::Closing);
        record.payment = Some(payment.clone());
        record.closing = Some(ClosingData {
            expiry_timestamp,
            balance: payment.balance,
            signature: payment.signature,
        });
        Ok(())
    }

    fn get_closing_data(&self, channel_id: &str) -> Option<ClosingData> {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.closing.clone())
    }

    fn get_channel_policy(&self, unit: &str) -> Option<ChannelPolicy> {
        if self.unit_policies.is_empty() {
            return Some(self.default_policy.clone());
        }
        self.unit_policies.get(unit).cloned()
    }

    fn now_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    fn get_balance_and_signature_for_unilateral_exit(
        &self,
        channel_id: &str,
    ) -> Option<PaymentProof> {
        lock(&self.channels)
            .get(channel_id)
            .and_then(|record| record.payment.clone())
    }

    fn get_active_keyset_ids(&self, mint: &str, unit: &CurrencyUnit) -> Vec<Id> {
        lock(&self.active_keysets)
            .get(&(mint.to_string(), unit.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn get_keyset_info(&self, mint: &str, keyset_id: &Id) -> Option<String> {
        lock(&self.keysets)
            .get(keyset_id)
            .filter(|entry| entry.mint == mint)
            .map(|entry| entry.info_json.clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_channel_closed(
        &self,
        channel_id: &str,
        expiry_timestamp: u64,
        balance: u64,
        receiver_proofs_json: &str,
        sender_proofs_json: &str,
        receiver_sum: u64,
        sender_sum: u64,
    ) -> Result<(), String> {
        let mut channels = lock(&self.channels);
        let record = channels
            .get_mut(channel_id)
            .ok_or_else(|| format!("unknown channel {channel_id}"))?;
        if record.state() == ChannelState::Closed {
            return Err(format!("channel {channel_id} is already closed"));
        }
        record.state = Some(ChannelState::Closed);
        record.settlement = Some(ChannelSettlement {
            expiry_timestamp,
            balance,
            receiver_sum,
            sender_sum,
            receiver_proofs_json: receiver_proofs_json.to_string(),
            sender_proofs_json: sender_proofs_json.to_string(),
        });
        Ok(())
    }

    /// ECDH: `channel_secret = hash(node_secret × sender_pubkey)`. Both parties
    /// derive the same value; only the receiver (us) needs the secret to build
    /// stage-2 outputs.
    fn compute_channel_secret(
        &self,
        receiver_pubkey_hex: &str,
        sender_pubkey_hex: &str,
    ) -> Result<String, String> {
        self.require_our_key("receiver", receiver_pubkey_hex)?;
        compute_channel_secret_from_hex(&self.node_secret_hex, sender_pubkey_hex)
    }

    /// BIP-340 Schnorr over the P2BK-tweaked node key (NUT-28 blinds the mint's
    /// view of our key, so the signature has to be made with the tweaked scalar).
    fn sign_with_tweaked_key(
        &self,
        signer_pubkey_hex: &str,
        message_hex: &str,
        tweak_scalar_hex: &str,
    ) -> Result<String, String> {
        self.require_our_key("signer", signer_pubkey_hex)?;
        sign_with_tweaked_key_util(&self.node_secret_hex, message_hex, tweak_scalar_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed, valid secp256k1 scalar (never use this outside tests).
    const NODE_SECRET_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    const SENDER_SECRET_HEX: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";
    const MINT: &str = "https://mint.test";
    const OTHER_MINT: &str = "https://other.mint.test";

    fn node_secret() -> SecretKey {
        SecretKey::from_hex(NODE_SECRET_HEX).expect("test node secret is a valid scalar")
    }

    fn sender_secret() -> SecretKey {
        SecretKey::from_hex(SENDER_SECRET_HEX).expect("test sender secret is a valid scalar")
    }

    fn keyset_id() -> Id {
        "009a1f293253e41e".parse().expect("valid v1 keyset id")
    }

    fn other_keyset_id() -> Id {
        // v1 keyset ids are a 0x00 version byte + 7 bytes (16 hex chars total).
        "00fedcba98765432".parse().expect("valid v1 keyset id")
    }

    fn funding() -> ChannelFunding {
        ChannelFunding {
            params_json: r#"{"capacity":1000}"#.to_string(),
            funding_proofs_json: "[]".to_string(),
            channel_secret_hex: "00".repeat(32),
            keyset_info_json: r#"{"keyset_id":"009a1f293253e41e"}"#.to_string(),
        }
    }

    fn payment(balance: u64) -> PaymentProof {
        PaymentProof {
            balance,
            signature: "ab".repeat(64),
        }
    }

    /// 1 sat per second, in milli-sat scale.
    fn one_sat_per_second() -> Price {
        Price {
            per_second: 1_000,
            per_unit: 0,
        }
    }

    fn host(price: Price) -> TollGateHost {
        TollGateHost::new(node_secret())
            .with_default_price(price)
            .with_accepted_mint(MINT)
    }

    #[test]
    fn funding_roundtrips_and_opens_the_channel() {
        let host = host(one_sat_per_second());
        host.save_funding("c1", funding(), payment(0));

        let stored = host.get_funding("c1").expect("funding is retrievable");
        assert_eq!(stored.params_json, r#"{"capacity":1000}"#);
        assert_eq!(host.channel_state("c1"), ChannelState::Open);
        // The initial balance is the unilateral-exit proof until a payment lands.
        let exit = host
            .get_balance_and_signature_for_unilateral_exit("c1")
            .expect("exit proof");
        assert_eq!(exit.balance, 0);
        assert!(host.get_funding("nope").is_none());
        assert_eq!(host.get_channel_state("nope"), ChannelState::Open);
    }

    #[test]
    fn receiver_key_defaults_to_accepting_any_and_can_be_pinned() {
        let open = host(Price::default());
        assert!(open.receiver_key_is_acceptable(&sender_secret().public_key()));

        let pinned = open.clone_with_receiver_key();
        assert!(pinned.receiver_key_is_acceptable(&node_secret().public_key()));
        assert!(!pinned.receiver_key_is_acceptable(&sender_secret().public_key()));
    }

    #[test]
    fn amount_due_is_rate_times_seconds_plus_units() {
        let metered = host(one_sat_per_second());
        metered.save_funding("c1", funding(), payment(0));

        // Metered time (out of band) + pending time from the request context.
        metered.record_usage("c1", 5_000, Counters::default());
        assert_eq!(metered.get_amount_due("c1", None), 5);

        let ctx = r#"{"seconds":2}"#.to_string();
        assert_eq!(metered.get_amount_due("c1", Some(&ctx)), 7);

        // Units are charged at per_unit and rounded up, never down.
        let per_unit_host = host(Price {
            per_second: 0,
            per_unit: 250, // milli-sat: 4 units = 1 sat exactly
        });
        per_unit_host.save_funding("c1", funding(), payment(0));
        let ctx = r#"{"units":3}"#.to_string();
        assert_eq!(per_unit_host.get_amount_due("c1", Some(&ctx)), 1); // ceil(750/1000)
        let ctx = r#"{"units":4}"#.to_string();
        assert_eq!(per_unit_host.get_amount_due("c1", Some(&ctx)), 1);
        let ctx = r#"{"units":8}"#.to_string();
        assert_eq!(per_unit_host.get_amount_due("c1", Some(&ctx)), 2);
    }

    #[test]
    fn wall_clock_billing_charges_time_since_funding() {
        let host = host(one_sat_per_second());
        host.save_funding("c1", funding(), payment(0));

        // Re-baseline the meter 10 seconds into the past: wall clock is billed.
        assert_eq!(host.current_amount_due("c1"), 0, "no time has passed yet");
        host.stash_baseline("c1", 10);
        assert_eq!(host.current_amount_due("c1"), 10);

        // Turning wall-clock billing off freezes the amount due.
        host.set_wall_clock_billing("c1", false);
        host.stash_baseline("c1", 30); // pretend 30 more seconds passed
        assert_eq!(host.current_amount_due("c1"), 0);
    }

    #[test]
    fn free_or_negative_pricing_is_never_billed() {
        let host = host(Price {
            per_second: -1_000,
            per_unit: -5,
        });
        host.save_funding("c1", funding(), payment(0));
        host.stash_baseline("c1", 60);
        host.record_usage(
            "c1",
            60_000,
            Counters {
                delivered: 100,
                received: 0,
            },
        );
        assert_eq!(host.current_amount_due("c1"), 0);
    }

    #[test]
    fn recording_a_payment_advances_usage_without_double_billing() {
        let host = host(one_sat_per_second());
        host.save_funding("c1", funding(), payment(0));

        // Ten seconds of wall-clock time are owed.
        host.stash_baseline("c1", 10);
        let due = host.current_amount_due("c1");
        assert_eq!(due, 10);
        assert!(!host.is_covered("c1"), "unpaid channel is not covered");

        // The bridge accepts a balance of 10 sats with 10 seconds of context.
        let ctx = r#"{"seconds":10}"#.to_string();
        host.record_payment("c1", payment(due), &ctx);

        assert_eq!(host.paid_balance("c1"), 10);
        assert!(host.is_covered("c1"));
        // Those 10 seconds are billed once: re-baselining on payment keeps the
        // amount due at 10 rather than doubling it to 20.
        assert_eq!(host.current_amount_due("c1"), 10);
    }

    #[test]
    fn channel_transitions_open_closing_closed() {
        let host = host(one_sat_per_second());
        host.save_funding("c1", funding(), payment(0));

        host.mark_channel_closing("c1", 1_700_000_000, payment(42))
            .expect("open channel can start closing");
        assert_eq!(host.channel_state("c1"), ChannelState::Closing);
        let closing = host.get_closing_data("c1").expect("closing data stored");
        assert_eq!(closing.balance, 42);
        assert_eq!(closing.expiry_timestamp, 1_700_000_000);

        host.mark_channel_closed("c1", 1_700_000_000, 42, "[\"r\"]", "[\"s\"]", 42, 958)
            .expect("closing channel can close");
        assert_eq!(host.channel_state("c1"), ChannelState::Closed);

        let settlement = host.settlement("c1").expect("settlement recorded");
        assert_eq!(settlement.balance, 42);
        assert_eq!(settlement.receiver_sum, 42);
        assert_eq!(settlement.sender_sum, 958);
        assert_eq!(settlement.receiver_proofs_json, "[\"r\"]");

        // Closing a closed channel, or closing an unknown one, is an error the
        // bridge surfaces rather than a silent state flip.
        assert!(
            host.mark_channel_closing("c1", 1, payment(1)).is_err(),
            "already closed"
        );
        assert!(
            host.mark_channel_closed("c1", 1, 1, "[]", "[]", 0, 0)
                .is_err()
        );
        assert!(host.mark_channel_closing("ghost", 1, payment(1)).is_err());
    }

    #[test]
    fn mints_and_keysets_fail_closed() {
        let host = host(one_sat_per_second());
        // Trusted mint, keyset not yet cached: accepted (the mint validates).
        assert!(host.mint_and_keyset_is_acceptable(MINT, &keyset_id()));
        // Untrusted mint: refused even with a cached keyset.
        assert!(!host.mint_and_keyset_is_acceptable(OTHER_MINT, &keyset_id()));

        host.add_keyset(MINT, &CurrencyUnit::Sat, keyset_id(), "{}", true);
        assert!(host.mint_and_keyset_is_acceptable(MINT, &keyset_id()));

        // A keyset rotated out at the mint is no longer acceptable.
        host.set_keyset_active(&keyset_id(), false);
        assert!(!host.mint_and_keyset_is_acceptable(MINT, &keyset_id()));
        host.set_keyset_active(&keyset_id(), true);

        // A keyset cached for another mint cannot be used against this one.
        host.add_keyset(
            OTHER_MINT,
            &CurrencyUnit::Sat,
            other_keyset_id(),
            "{}",
            true,
        );
        assert!(!host.mint_and_keyset_is_acceptable(MINT, &other_keyset_id()));
    }

    #[test]
    fn keyset_cache_is_queryable_like_a_storage_backend() {
        let host = host(one_sat_per_second());
        host.add_keyset(
            MINT,
            &CurrencyUnit::Sat,
            keyset_id(),
            r#"{"keyset_id":"009a1f293253e41e"}"#,
            true,
        );

        assert_eq!(host.keyset_count(), 1);
        assert_eq!(
            host.get_active_keyset_ids(MINT, &CurrencyUnit::Sat),
            vec![keyset_id()]
        );
        assert!(
            host.get_active_keyset_ids(MINT, &CurrencyUnit::Msat)
                .is_empty()
        );
        assert!(
            host.get_active_keyset_ids(OTHER_MINT, &CurrencyUnit::Sat)
                .is_empty()
        );
        assert_eq!(
            host.get_keyset_info(MINT, &keyset_id()).as_deref(),
            Some(r#"{"keyset_id":"009a1f293253e41e"}"#)
        );
        assert!(host.get_keyset_info(OTHER_MINT, &keyset_id()).is_none());
    }

    #[test]
    fn channel_policy_is_per_unit_once_units_are_pinned() {
        let permissive = host(Price::default());
        assert!(permissive.get_channel_policy("sat").is_some());

        let pinned = permissive.clone_with_unit_policy(
            "sat",
            ChannelPolicy {
                min_capacity: 64,
                min_expiry_in_seconds: 7_200,
                max_amount_per_output: Some(64),
            },
        );
        let policy = pinned.get_channel_policy("sat").expect("sat is pinned");
        assert_eq!(policy.min_capacity, 64);
        assert_eq!(policy.min_expiry_in_seconds, 7_200);
        assert_eq!(policy.max_amount_per_output, Some(64));
        // With an allow-list in place, unlisted units are refused.
        assert!(pinned.get_channel_policy("msat").is_none());
    }

    #[test]
    fn channel_secret_is_the_shared_ecdh_point() {
        let host = host(Price::default());
        let sender_pubkey_hex = sender_secret().public_key().to_hex();

        let ours = host
            .compute_channel_secret(host.node_pubkey_hex(), &sender_pubkey_hex)
            .expect("channel addressed to this node");
        let theirs = compute_channel_secret_from_hex(SENDER_SECRET_HEX, host.node_pubkey_hex())
            .expect("cdk-spilman derives the same secret from the sender's side");

        assert_eq!(ours, theirs);
        assert_eq!(ours.len(), 64, "32-byte secret, hex encoded");

        // A channel addressed to somebody else cannot be settled by us.
        let foreign = sender_secret().public_key().to_hex();
        let err = host
            .compute_channel_secret(&foreign, &sender_pubkey_hex)
            .expect_err("foreign receiver key is refused");
        assert!(
            err.contains("not this node's key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tweaked_signature_verifies_against_the_tweaked_key() {
        let host = host(Price::default());
        let node_pubkey_hex = host.node_pubkey_hex().to_string();
        let message_hex = "33".repeat(32);
        let tweak_hex = "44".repeat(32);

        let signature_hex = host
            .sign_with_tweaked_key(&node_pubkey_hex, &message_hex, &tweak_hex)
            .expect("node can sign with its own tweaked key");
        assert_eq!(signature_hex.len(), 128, "64-byte BIP-340 signature, hex");

        // Verify independently: tweaked = (node secret, BIP-340 parity folded) +
        // tweak, then check the Schnorr signature against that x-only key.
        let node_bytes: [u8; 32] = hex_to_array(NODE_SECRET_HEX);
        let mut secret = secp256k1::SecretKey::from_slice(&node_bytes).expect("valid scalar");
        if secret
            .public_key(&secp256k1::Secp256k1::new())
            .x_only_public_key()
            .1
            == secp256k1::Parity::Odd
        {
            secret = secret.negate();
        }
        let tweak =
            secp256k1::Scalar::from_be_bytes(hex_to_array(&tweak_hex)).expect("valid tweak scalar");
        let tweaked = secret.add_tweak(&tweak).expect("tweak adds");
        let secp = secp256k1::Secp256k1::new();
        let keypair = secp256k1::Keypair::from_secret_key(&secp, &tweaked);
        let (xonly, _) = keypair.x_only_public_key();
        let signature = secp256k1::schnorr::Signature::from_slice(&hex_to_vec(&signature_hex))
            .expect("signature parses");
        // secp256k1 0.30 takes the digest as bytes here.
        secp.verify_schnorr(&signature, &hex_to_array(&message_hex), &xonly)
            .expect("signature verifies against the tweaked key");

        // Signing as somebody else is refused, and malformed inputs surface as
        // errors rather than panics.
        let foreign = sender_secret().public_key().to_hex();
        assert!(
            host.sign_with_tweaked_key(&foreign, &message_hex, &tweak_hex)
                .is_err()
        );
        assert!(
            host.sign_with_tweaked_key(&node_pubkey_hex, "not-hex", &tweak_hex)
                .is_err()
        );
        assert!(
            host.sign_with_tweaked_key(&node_pubkey_hex, &message_hex, "abcd")
                .is_err()
        );
    }

    #[test]
    fn registered_price_overrides_the_default_and_usage_accumulates() {
        let host = host(one_sat_per_second());
        host.save_funding("c1", funding(), payment(0));
        // Negotiated rate: 2 sats per second, billed per delivered unit instead
        // of wall clock, so only what the metering loop reports is charged.
        host.register_channel_meter(
            "c1",
            Price {
                per_second: 2_000,
                per_unit: 1_000,
            },
            1_000,
        );
        host.set_wall_clock_billing("c1", false);

        host.record_usage(
            "c1",
            3_000,
            Counters {
                delivered: 4,
                received: 99,
            },
        );
        host.record_usage(
            "c1",
            2_000,
            Counters {
                delivered: 6,
                received: 0,
            },
        );
        // 5 s × 2 sat + 10 units × 1 sat = 20 sats.
        assert_eq!(host.current_amount_due("c1"), 20);
        assert_eq!(
            host.channel_meter("c1").expect("meter").price().per_second,
            2_000
        );

        let ctx = r#"{"seconds":1,"units":2}"#.to_string();
        assert_eq!(host.get_amount_due("c1", Some(&ctx)), 24);
    }

    #[test]
    fn unknown_channels_owe_nothing_and_can_be_listed() {
        let host = host(one_sat_per_second());
        assert_eq!(host.current_amount_due("ghost"), 0);
        host.save_funding("c1", funding(), payment(0));
        host.save_funding("c2", funding(), payment(0));
        assert_eq!(host.channel_ids(), vec!["c1".to_string(), "c2".to_string()]);
        assert_eq!(host.paid_balance("ghost"), 0);
    }

    fn hex_to_vec(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    fn hex_to_array(hex: &str) -> [u8; 32] {
        hex_to_vec(hex).try_into().expect("32 bytes")
    }

    impl TollGateHost {
        /// Test-only: pretend the billing baseline was `seconds_ago` in the past.
        fn stash_baseline(&self, channel_id: &str, seconds_ago: u64) {
            let mut channels = lock(&self.channels);
            let record = channels
                .get_mut(channel_id)
                .expect("channel exists in test");
            let meter = record.meter.as_mut().expect("meter exists in test");
            let baseline = self.now_seconds().saturating_sub(seconds_ago);
            meter.rebaseline(baseline);
        }

        fn clone_with_receiver_key(&self) -> Self {
            Self {
                node_secret: self.node_secret.clone(),
                node_secret_hex: self.node_secret_hex.clone(),
                node_pubkey_hex: self.node_pubkey_hex.clone(),
                accepted_mints: self.accepted_mints.clone(),
                pricing_scale: self.pricing_scale,
                default_price: self.default_price,
                default_policy: self.default_policy.clone(),
                unit_policies: self.unit_policies.clone(),
                accepted_receiver_keys: vec![self.node_pubkey()],
                channels: Mutex::new(HashMap::new()),
                keysets: Mutex::new(HashMap::new()),
                active_keysets: Mutex::new(HashMap::new()),
            }
        }

        fn clone_with_unit_policy(&self, unit: &str, policy: ChannelPolicy) -> Self {
            let mut clone = self.clone_with_receiver_key();
            clone.unit_policies.insert(unit.to_string(), policy);
            clone
        }
    }
}
