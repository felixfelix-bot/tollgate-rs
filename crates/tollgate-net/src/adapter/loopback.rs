//! A shaper and meter over a dedicated socket, in userspace.
//!
//! It forwards nobody's traffic — the bytes it governs are produced by
//! [`crate::dataplane`] rather than by somebody wanting them — but the shaping
//! and the accounting are real, which is what lets the whole protocol be
//! exercised on any platform.
//!
//! The shaper is a token bucket. A grant buys a rate that is fixed for the
//! grant's life, so a bucket refilling at that rate and capped at a short burst
//! is the natural enforcement: capacity left unused early is not banked, which
//! is exactly what the cap expresses.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use tollgate_core::access::AccessLevel;
use tollgate_core::grant::units_in;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;

use super::ResourceAdapter;

/// How much of an unused second the bucket will hold onto.
///
/// Some slack absorbs scheduling jitter — a writer that wakes 30 ms late should
/// still be able to send what it was owed. Too much and a buyer could idle and
/// then burst, which is the thing grants exist to prevent, so this stays well
/// under one window.
const BURST_MS: u64 = 250;

/// One peer's link, as the adapter sees it.
#[derive(Debug, Clone, Copy)]
struct Link {
    access: AccessLevel,
    /// Units per second we may deliver to this peer.
    rate: u64,
    /// Tokens available to spend right now.
    tokens: u64,
    /// Cumulative units delivered to and received from this peer, since the
    /// session started. Raw — the received multiplier is applied by core when
    /// it draws the grant down, not here.
    counters: Counters,
    /// Units per second *we* want to pull from this peer. Drives our buying.
    demand: u64,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            access: AccessLevel::None,
            rate: 0,
            tokens: 0,
            counters: Counters::ZERO,
            demand: 0,
        }
    }
}

/// A shaper and meter over a dedicated socket.
#[derive(Debug, Default)]
pub struct Loopback {
    links: Mutex<HashMap<PubKey, Link>>,
}

impl Loopback {
    /// An adapter with no peers.
    pub fn new() -> Self {
        Self::default()
    }
}

impl ResourceAdapter for Loopback {
    /// Nothing is gated by address here, so the address is not needed.
    fn register(&self, _peer: PubKey, _addr: IpAddr) {}

    fn set_access(&self, peer: PubKey, access: AccessLevel) {
        self.links
            .lock()
            .expect("not poisoned")
            .entry(peer)
            .or_default()
            .access = access;
    }

    fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.rate = rate;
        // Do not carry a fat bucket across a rate change: a peer that just
        // bought a much higher rate should not also get to spend tokens that
        // accrued while it was idle at the old one.
        link.tokens = link.tokens.min(units_in(rate, BURST_MS));
    }

    fn set_demand(&self, peer: PubKey, rate: u64) {
        self.links
            .lock()
            .expect("not poisoned")
            .entry(peer)
            .or_default()
            .demand = rate;
    }

    fn demand(&self, peer: PubKey) -> u64 {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.demand)
            .unwrap_or(0)
    }

    fn counters(&self, peer: PubKey) -> Counters {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.counters)
            .unwrap_or(Counters::ZERO)
    }

    fn shaping_rate(&self, peer: PubKey) -> u64 {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.rate)
            .unwrap_or(0)
    }

    fn peers(&self) -> Vec<PubKey> {
        self.links
            .lock()
            .expect("not poisoned")
            .keys()
            .copied()
            .collect()
    }

    fn remove(&self, peer: PubKey) {
        self.links.lock().expect("not poisoned").remove(&peer);
    }
}

impl Loopback {
    /// Refill a peer's bucket for `elapsed_ms` and take out what may be sent now.
    ///
    /// Returns the number of units the data plane is allowed to write. A peer
    /// whose delivery is blocked gets nothing regardless of its bucket.
    pub fn take_allowance(&self, peer: PubKey, elapsed_ms: u64) -> u64 {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();

        if !link.access.delivery_allowed() && link.rate == 0 {
            return 0;
        }

        let cap = units_in(link.rate, BURST_MS);
        link.tokens = link
            .tokens
            .saturating_add(units_in(link.rate, elapsed_ms))
            .min(cap);

        let take = link.tokens;
        link.tokens -= take;
        take
    }

    /// Record units actually written to a peer — its download.
    pub fn record_delivered(&self, peer: PubKey, units: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.counters.delivered = link.counters.delivered.saturating_add(units);
    }

    /// Record units actually read from a peer — its upload.
    pub fn record_received(&self, peer: PubKey, units: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.counters.received = link.counters.received.saturating_add(units);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PubKey {
        PubKey([0x02; 33])
    }

    #[test]
    fn the_bucket_refills_at_the_shaped_rate() {
        let adapter = Loopback::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000_000);

        assert_eq!(
            adapter.take_allowance(peer(), 100),
            100_000,
            "100 ms at 1 M/s"
        );
        assert_eq!(adapter.take_allowance(peer(), 50), 50_000);
    }

    #[test]
    fn an_idle_peer_cannot_bank_capacity_and_then_burst() {
        // This is the token bucket standing in for "capacity left unused early
        // is not banked for later".
        let adapter = Loopback::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000_000);

        let after_idling = adapter.take_allowance(peer(), 60_000);
        assert_eq!(
            after_idling,
            units_in(1_000_000, BURST_MS),
            "a minute of idling buys only the burst allowance"
        );
    }

    #[test]
    fn a_rate_rise_does_not_release_tokens_banked_at_the_old_rate() {
        let adapter = Loopback::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000);
        adapter.take_allowance(peer(), 10_000);

        adapter.set_shaping_rate(peer(), 100_000_000);
        assert_eq!(
            adapter.take_allowance(peer(), 0),
            0,
            "the bucket was trimmed to the new rate's burst, and nothing has accrued yet"
        );
    }

    #[test]
    fn a_blocked_peer_with_no_allowance_gets_nothing() {
        let adapter = Loopback::new();
        adapter.set_access(peer(), AccessLevel::None);
        adapter.set_shaping_rate(peer(), 0);
        assert_eq!(adapter.take_allowance(peer(), 1_000), 0);
    }

    #[test]
    fn a_blocked_peer_still_gets_the_minimum_flow_allowance() {
        // The allowance is what breaks the bootstrap circle: a peer holding no
        // vouchers has to be able to reach a mint to acquire some.
        let adapter = Loopback::new();
        adapter.set_access(peer(), AccessLevel::None);
        adapter.set_shaping_rate(peer(), 4_096);
        assert_eq!(
            adapter.take_allowance(peer(), 1_000),
            1_024,
            "capped by the burst"
        );
    }

    #[test]
    fn counters_accumulate_in_both_directions() {
        let adapter = Loopback::new();
        adapter.record_delivered(peer(), 1_000);
        adapter.record_delivered(peer(), 500);
        adapter.record_received(peer(), 200);

        assert_eq!(
            adapter.counters(peer()),
            Counters {
                delivered: 1_500,
                received: 200
            }
        );
    }
}
