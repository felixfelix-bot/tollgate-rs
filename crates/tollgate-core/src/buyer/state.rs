//! What this node has bought from one peer, and the policy it buys under.
//!
//! The Spilman ratchet is **per channel** — a cumulative total only means
//! anything against the channel it was signed on — so the payer tracks up to
//! three at once:
//!
//! ```text
//! active    being drained now
//! next      funded and confirmed, waiting for `active` to fill
//! pending   funded by us, not yet confirmed by the peer
//! ```
//!
//! A channel is opened well before it is needed (default: at 80% of the one in
//! use) precisely so that `next` is ready by the time a purchase overflows
//! `active`, and the overflow can be signed across both rather than stalling.

use tollgate_protocol::ChannelId;

use crate::time::Millis;

/// How aggressively to buy. Deliberately simple — the design leaves window
/// choice open ("the payer trades responsiveness against forfeiture and message
/// count, with no obvious default"), so this is one workable policy rather than
/// the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyerPolicy {
    /// Buy this percentage of observed demand. Above 100 leaves headroom so a
    /// rising flow is not shaped before the next purchase lands.
    pub headroom_pct: u32,
    /// Only jump mid-window if the target exceeds the rate in force by this
    /// percentage.
    ///
    /// This is the hysteresis that makes the forfeiture rule livable: raising
    /// the rate early burns the remainder, and the design points out that large
    /// jumps are cheap while small adjustments are punitive. A high threshold
    /// is what stops the buyer fiddling.
    pub raise_threshold_pct: u32,
    /// Renew this long before the deadline, so the next grant lands before the
    /// current one lapses and the peer drops to the minimum flow allowance.
    ///
    /// This is the one knob measured in absolute time rather than in
    /// proportion, and it is the one that decides whether a real flow survives.
    /// See [`BuyerPolicy::MIN_SAFE_LEAD_MS`].
    pub renew_lead_ms: u32,
    /// How long to respect a rate ceiling a provider named before testing
    /// whether capacity has freed up.
    pub cap_hold_ms: u64,
    /// Window to ask for, clamped to what the provider advertised.
    ///
    /// Short windows keep the forfeit small and reaction quick, at the cost of
    /// more signature verifications for the provider.
    pub window_ms: u32,
    /// Never buy below this rate, so an idle link keeps a little capacity ready.
    pub min_rate: u64,
    /// Never buy above this rate. The operator's spending ceiling — vouchers
    /// cost money to acquire, whatever the protocol thinks.
    pub max_rate: u64,
}

impl BuyerPolicy {
    /// A renewal lead below this is asking for a lapsed grant.
    ///
    /// The lead is how late a renewal may be and still land before the grant it
    /// replaces runs out — an absolute tolerance, in milliseconds, against
    /// everything between deciding to buy and the provider applying the result:
    /// a signature, a round trip, the provider's tick, a scheduler that had
    /// something else to do. A node under load misses a few hundred
    /// milliseconds without much trying.
    ///
    /// Missing it is expensive out of all proportion to the gap. The grant
    /// lapses, the shaper drops to the minimum flow allowance with a window's
    /// worth of packets in flight, and a TCP flow crossing that spends seconds
    /// in backoff recovering from a gap of a tenth of a second. Measured, in
    /// `testing/forwarding`: a 300 ms lead flaked under load; 1.2 s held.
    ///
    /// A thousand is not a fact about the protocol. It is the smallest lead
    /// that has survived a loaded machine here.
    pub const MIN_SAFE_LEAD_MS: u32 = 1_000;

    /// The lead this policy can actually use inside a window of `window_ms`.
    ///
    /// The window is not entirely the payer's to choose — the provider bounds
    /// it, and a short bound can leave a configured lead longer than the window
    /// it renews inside. Taken literally that renews continuously: every grant
    /// is already inside its own lead the moment it starts.
    ///
    /// Half the window is the ceiling. Past that a renewal forfeits more of the
    /// grant than it keeps, which is a worse answer than renewing late.
    pub fn lead_within(&self, window_ms: u32) -> u32 {
        self.renew_lead_ms.min(window_ms / 2)
    }

    /// What renewing early costs, as a percentage of each grant.
    ///
    /// The forfeit is the lead over the window: buying again abandons whatever
    /// is left of the grant in force. Absolute tolerance is bought with a
    /// proportion of every grant, which is why scaling both together is free
    /// and shortening the window is not.
    pub fn forfeit_pct(&self, window_ms: u32) -> u32 {
        if window_ms == 0 {
            return 100;
        }
        self.lead_within(window_ms).saturating_mul(100) / window_ms
    }

    /// Whether the lead is too short to absorb ordinary scheduling jitter.
    pub fn lead_is_thin(&self) -> bool {
        self.renew_lead_ms < Self::MIN_SAFE_LEAD_MS
    }
}

impl Default for BuyerPolicy {
    /// A window and a lead that carry a real TCP flow.
    ///
    /// 1.2 s of tolerance for 30% of each grant. Both numbers were arrived at
    /// by measurement rather than by taste: shorter leads lapse under load, and
    /// the pair scales — a wider window at the same ratio buys more absolute
    /// slack for the same proportional forfeit, at the cost of reacting to a
    /// change in demand one window later.
    fn default() -> Self {
        Self {
            headroom_pct: 125,
            raise_threshold_pct: 150,
            renew_lead_ms: 1_200,
            cap_hold_ms: 10_000,
            window_ms: 4_000,
            min_rate: 0,
            max_rate: u64::MAX,
        }
    }
}

/// Window bounds the provider advertised in its Offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBounds {
    /// Smallest window the provider will accept.
    pub min_ms: u32,
    /// Largest window the provider will accept.
    pub max_ms: u32,
}

impl WindowBounds {
    /// Fit a preferred window into what the provider will take.
    pub fn clamp(&self, want_ms: u32) -> u32 {
        want_ms.clamp(self.min_ms, self.max_ms)
    }
}

/// One channel, from the payer's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelBuyer {
    /// The channel.
    pub id: ChannelId,
    /// Units it can carry in total.
    pub capacity: u64,
    /// Cumulative units signed **on this channel**. Monotonic, and meaningless
    /// against any other channel.
    pub cumulative: u64,
}

impl ChannelBuyer {
    /// A freshly funded channel, nothing signed on it yet.
    pub fn new(id: ChannelId, capacity: u64) -> Self {
        Self {
            id,
            capacity,
            cumulative: 0,
        }
    }

    /// Units that can still be signed onto this channel.
    pub fn headroom(&self) -> u64 {
        self.capacity.saturating_sub(self.cumulative)
    }

    /// Whether the channel has been drained to its capacity.
    pub fn exhausted(&self) -> bool {
        self.cumulative >= self.capacity
    }
}

/// One channel update a purchase requires.
///
/// A purchase that overflows the channel in use takes two, because a cumulative
/// total is only meaningful against the channel it was signed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    /// The channel to ratchet.
    pub channel_id: ChannelId,
    /// New cumulative total on that channel.
    pub cumulative: u64,
}

/// A purchase decided but not yet sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purchase {
    /// The update on the channel currently in use.
    pub first: Leg,
    /// The update on the replacement channel, when the purchase overflowed the
    /// first. Present only when a rollover is already funded and confirmed.
    pub second: Option<Leg>,
    /// Window to spend it in, already clamped to the provider's range.
    pub window_ms: u32,
    /// Rate this buys.
    pub rate: u64,
    /// Units bought by this purchase alone, across both legs.
    pub grant: u64,
    /// Units of the previous grant given up to make this one. Zero on a
    /// renewal that waited for the deadline; the price of reacting early
    /// otherwise. Worth logging — it is the cost the policy is trading against.
    pub forfeited: u64,
    /// Why the buyer acted, for the operator's benefit.
    pub trigger: Trigger,
}

/// What prompted a purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// No grant has been bought yet on this channel.
    First,
    /// The grant in force is about to lapse.
    Renewal,
    /// Demand climbed far enough to be worth forfeiting the remainder for.
    DemandRose,
    /// The provider refused the last purchase and named a rate it would take.
    Rebuy,
}

/// The buyer's channel and grant state before a purchase, kept so a rejection
/// can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prior {
    pub(super) active: Option<ChannelBuyer>,
    pub(super) next: Option<ChannelBuyer>,
    pub(super) rate: u64,
    pub(super) deadline: Millis,
    pub(super) started: bool,
}

/// The payer's side of one peering.
#[derive(Debug, Clone, Copy, Default)]
pub struct Buyer {
    /// The channel being drained.
    pub(super) active: Option<ChannelBuyer>,
    /// A confirmed replacement, waiting for `active` to fill.
    pub(super) next: Option<ChannelBuyer>,
    /// Funded by us, not yet confirmed by the peer. Nothing is signed on it —
    /// the peer has not said it verified the funding, so a grant signed here
    /// might be against a channel that never opens.
    pub(super) pending: Option<ChannelBuyer>,
    /// Rate the grant in force bought.
    pub(super) rate: u64,
    /// When it lapses.
    pub(super) deadline: Millis,
    /// Whether anything has been bought yet.
    pub(super) started: bool,
    /// Ceiling the provider last told us it would honor, from a TopUpReject,
    /// and when it is worth testing again.
    ///
    /// Held for a while rather than forgotten on the next purchase: capacity
    /// may free up, but re-probing every window means a peer parked above the
    /// cap is refused once per window forever, which walks straight through the
    /// signature-verification budget `min_window_ms` exists to protect.
    pub(super) capped_at: Option<(u64, Millis)>,
    /// Units bought by the most recent purchase, so a rollover can be started
    /// before a channel is too small to carry another one.
    pub(super) last_grant: u64,
    /// State as it stood before the most recent purchase.
    ///
    /// A TopUp is fire-and-forget, so we assume it landed and advance. If a
    /// TopUpReject comes back, the provider never turned its ratchet — and if
    /// we kept our own advanced, every subsequent purchase would be computed
    /// from a total the provider does not recognise, and would be refused for
    /// the same reason as the first. So we keep exactly one step of history to
    /// undo.
    pub(super) prior: Option<Prior>,
}

impl Buyer {
    /// A buyer with no channel yet.
    pub const fn new() -> Self {
        Self {
            active: None,
            next: None,
            pending: None,
            rate: 0,
            deadline: Millis::ZERO,
            started: false,
            capped_at: None,
            last_grant: 0,
            prior: None,
        }
    }

    /// The channel being drained.
    pub fn active(&self) -> Option<ChannelBuyer> {
        self.active
    }

    /// The confirmed replacement, if one is ready.
    pub fn next_channel(&self) -> Option<ChannelBuyer> {
        self.next
    }

    /// Whether a channel has been funded and is awaiting the peer's
    /// confirmation. While this is set, no further rollover is started.
    pub fn awaiting_confirmation(&self) -> bool {
        self.pending.is_some()
    }

    /// Cumulative units signed on the channel in use. Only meaningful against
    /// that channel.
    pub fn cumulative(&self) -> u64 {
        self.active.map(|c| c.cumulative).unwrap_or(0)
    }

    /// The rate currently bought.
    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// When the grant in force lapses.
    pub fn deadline(&self) -> Millis {
        self.deadline
    }

    /// The rate ceiling in force, if a provider named one recently enough.
    pub(super) fn cap(&self, now: Millis) -> Option<u64> {
        self.capped_at
            .filter(|(_, until)| now < *until)
            .map(|(rate, _)| rate)
    }

    /// Record a channel we have funded but the peer has not yet confirmed.
    pub fn funded(&mut self, id: ChannelId, capacity: u64) {
        self.pending = Some(ChannelBuyer::new(id, capacity));
    }

    /// The peer confirmed the channel we funded.
    ///
    /// It becomes the one in use if there is none, and otherwise the
    /// replacement waiting behind it.
    pub fn confirmed(&mut self, id: ChannelId) {
        let Some(mut channel) = self.pending.take() else {
            return;
        };
        // The peer names the channel it verified, which is what the funding
        // determined; trust it over what we derived locally.
        channel.id = id;

        if self.active.is_none() {
            self.active = Some(channel);
        } else {
            self.next = Some(channel);
        }
    }

    /// Whether to open a replacement for the channel in use.
    ///
    /// Rollover is started by the funder alone — only the party putting up new
    /// funds decides when — so this is only ever asked about our own channel.
    /// It stays false while one is already on the way, or the threshold would
    /// re-trigger on every check and fund a channel each time.
    ///
    /// Two triggers, and the second is the one that matters in practice:
    ///
    /// 1. **Past the threshold.** The channel is far enough through its
    ///    capacity to be worth replacing.
    /// 2. **Not enough headroom for another purchase like the last one.**
    ///    A threshold alone is reactive, and a purchase is not gradual: a grant
    ///    can take a channel from empty to full in a single step, and then
    ///    there is nothing to move onto. The peer falls to the minimum flow
    ///    allowance while a replacement is funded and verified — a round trip
    ///    plus a mint swap — which is a visible stall for something entirely
    ///    predictable.
    pub fn needs_rollover(&self, threshold_pct: u8) -> bool {
        if self.next.is_some() || self.pending.is_some() {
            return false;
        }
        let Some(active) = self.active else {
            return false;
        };
        if active.capacity == 0 {
            return false;
        }

        // Room for more than the next purchase and the one after it. Exactly
        // two is already too late: the replacement has to be funded, sent,
        // verified and confirmed before the channel in use runs dry, and that
        // is a round trip plus a mint swap.
        if active.headroom() <= self.last_grant.saturating_mul(2) {
            return true;
        }

        // Widened rather than saturated: saturating either side would make a
        // large channel look permanently past its threshold.
        (active.cumulative as u128) * 100 >= (active.capacity as u128) * (threshold_pct as u128)
    }

    /// Units of the grant in force still unspent from our side's point of view
    /// — an upper bound, since we cannot see the provider's counters.
    pub fn unspent_at(&self, now: Millis) -> u64 {
        if !self.started || now >= self.deadline {
            return 0;
        }
        crate::grant::units_in(self.rate, self.deadline.saturating_since(now))
    }

    /// Commit to a purchase we have decided to send.
    ///
    /// Returns the channel that this purchase exhausted, if any. That channel
    /// has been drained to its capacity and can be settled — its replacement is
    /// already carrying the overflow.
    pub fn record(&mut self, purchase: Purchase, now: Millis) -> Option<ChannelId> {
        self.prior = Some(Prior {
            active: self.active,
            next: self.next,
            rate: self.rate,
            deadline: self.deadline,
            started: self.started,
        });

        if let Some(active) = self.active.as_mut() {
            debug_assert_eq!(active.id, purchase.first.channel_id);
            active.cumulative = purchase.first.cumulative;
        }
        if let Some(leg) = purchase.second
            && let Some(next) = self.next.as_mut()
        {
            debug_assert_eq!(next.id, leg.channel_id);
            next.cumulative = leg.cumulative;
        }

        self.rate = purchase.rate;
        self.deadline = now + purchase.window_ms as u64;
        self.started = true;
        self.last_grant = purchase.grant;

        // A purchase at or under the cap does not disprove it, so the cap
        // stands until it expires on its own.
        self.retire_exhausted()
    }

    /// Move on from a channel drained to its capacity.
    fn retire_exhausted(&mut self) -> Option<ChannelId> {
        let active = self.active?;
        if !active.exhausted() {
            return None;
        }
        // Only step forward once the replacement is actually there. Without one
        // there is nothing to step to, and the buyer stops buying until the
        // rollover completes — which is the correct outcome, not a stall: the
        // channel really is full.
        let replacement = self.next.take()?;
        self.active = Some(replacement);
        Some(active.id)
    }

    /// Record that the provider refused a purchase, and at what rate it said it
    /// would accept one.
    ///
    /// The refusal costs nothing directly — an unclaimed state is worth nothing
    /// to the provider, so our money is untouched. What it buys us is the rate
    /// to re-purchase at, in one round trip.
    ///
    /// Because the provider did not turn its ratchet, we undo ours: the next
    /// purchase has to be built on the last total the provider actually
    /// accepted, or it would be refused for exactly the same reason.
    /// `refused` identifies which purchase was refused, so a stale rejection
    /// for one we have already moved past only records the cap and does not
    /// rewind anything.
    pub fn record_reject(
        &mut self,
        refused: &[(ChannelId, u64)],
        max_rate: u64,
        now: Millis,
        hold_ms: u64,
    ) {
        self.capped_at = Some((max_rate, now + hold_ms));

        // A purchase is refused in full, so it is enough that any of the totals
        // named is one we currently hold — they were all sent together.
        let holds = |c: Option<ChannelBuyer>| {
            c.is_some_and(|c| {
                refused
                    .iter()
                    .any(|(id, cum)| *id == c.id && *cum == c.cumulative)
            })
        };
        if !holds(self.active) && !holds(self.next) {
            return;
        }
        if let Some(prior) = self.prior.take() {
            self.active = prior.active;
            self.next = prior.next;
            self.rate = prior.rate;
            self.deadline = prior.deadline;
            self.started = prior.started;
        }
    }
}
