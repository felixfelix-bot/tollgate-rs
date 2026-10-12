//! The driver: turns real events into [`Event`]s, runs [`Action`]s.
//!
//! This is the whole of the host's job. Core decides; this connects those
//! decisions to sockets, a clock, a signer and a channel backend. Everything
//! that would stop the crate below from running on an ESP32 lives here.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{NodePolicy, PeerPolicy};
use tollgate_core::session::Sessions;
use tollgate_core::{Action, Event, Millis};
use tollgate_protocol::{ChannelUpdate, Message, PubKey, TopUp, TopUpReject};
use tracing::{debug, info, warn};

use crate::adapter::ResourceAdapter;
use crate::channel::ChannelBackend;
use crate::control;
use crate::identity::Identity;
use crate::wire::{self, Identify, Wire};

/// How often the node samples its meters and ticks core.
///
/// The meter sample is what draws grants down, so this also bounds how far past
/// its grant a peer can get before the shaper notices. It has to be well under
/// the smallest grant window a payer can ask for.
const TICK: Duration = Duration::from_millis(100);

/// A peer the operator has said something about.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Their identity, known ahead of time.
    pub pubkey: PubKey,
    /// `host:port` of their control plane, if we are the side that dials. The
    /// data plane is the next port up.
    ///
    /// `None` for a peer that dials us. Its policy still applies — refusing a
    /// peer, or carrying it for free, is a decision about who it is and not
    /// about who opened the connection.
    pub endpoint: Option<String>,
    /// Operator overrides for this peer.
    pub policy: PeerPolicy,
}

/// Everything a node needs to start.
#[derive(Debug)]
pub struct NodeConfig {
    /// This node's keypair.
    pub identity: Identity,
    /// What it sells and on what terms.
    pub policy: NodePolicy,
    /// How it buys.
    pub buyer: BuyerPolicy,
    /// Control-plane listen address.
    pub listen: SocketAddr,
    /// Whether a peer's announced key has to agree with the address it
    /// connects from.
    pub identify: Identify,
    /// Where this node serves its own mint.
    pub mint_listen: SocketAddr,
    /// The URL peers reach that mint on, advertised in our Offer.
    pub mint_url: String,
    /// Peers to dial. Anyone else has to dial us.
    pub peers: Vec<PeerConfig>,
}

impl NodeConfig {
    /// Where the loopback data plane listens, one port above the control plane.
    ///
    /// Only meaningful for the loopback adapter: a kernel adapter forwards real
    /// traffic and has no socket of its own.
    pub fn data_listen(&self) -> SocketAddr {
        let mut addr = self.listen;
        addr.set_port(self.listen.port() + 1);
        addr
    }
}

/// A running node.
pub struct Node {
    identity: Identity,
    sessions: Sessions,
    adapter: Arc<dyn ResourceAdapter>,
    channels: Arc<dyn ChannelBackend>,
    /// Outbound queue per connected peer.
    links: HashMap<PubKey, mpsc::Sender<Message>>,
    /// What the node is doing, republished each tick for the control socket.
    published: control::Published,
    started: Instant,
}

impl Node {
    /// Build a node. Nothing is listening or dialing until [`Self::run`].
    pub fn new(
        config: &NodeConfig,
        channels: Arc<dyn ChannelBackend>,
        adapter: Arc<dyn ResourceAdapter>,
    ) -> Self {
        let mut sessions = Sessions::new(
            config.identity.pubkey(),
            config.policy.clone(),
            config.buyer,
        );
        for peer in &config.peers {
            sessions.set_peer_policy(peer.pubkey, peer.policy);
        }

        Self {
            identity: config.identity.clone(),
            sessions,
            adapter,
            channels,
            links: HashMap::new(),
            published: Default::default(),
            started: Instant::now(),
        }
    }

    /// The snapshot the control socket serves. Cheap to clone and lock-free to
    /// read, so a watcher never holds the event loop up.
    pub fn published(&self) -> control::Published {
        Arc::clone(&self.published)
    }

    /// The adapter, so a demo or a test can set demand and read counters.
    pub fn adapter(&self) -> Arc<dyn ResourceAdapter> {
        Arc::clone(&self.adapter)
    }

    /// This node's identity.
    pub fn pubkey(&self) -> PubKey {
        self.identity.pubkey()
    }

    /// Milliseconds since this node started.
    ///
    /// Core never reads a clock; this is the only place time enters, and it is
    /// monotonic by construction.
    fn now(&self) -> Millis {
        Millis(self.started.elapsed().as_millis() as u64)
    }

    /// Listen, dial, and run until `shutdown` resolves or something goes badly
    /// wrong.
    pub async fn run(
        mut self,
        config: NodeConfig,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<()> {
        let (wire_tx, mut wire_rx) = mpsc::channel::<Wire>(256);
        // Channel work may block — a real backend talks to a mint — so it runs
        // on a blocking thread and its result comes back here rather than
        // stalling the event loop.
        let (done_tx, mut done_rx) = mpsc::channel::<Event>(256);

        let control = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("bind control plane on {}", config.listen))?;
        info!(
            pubkey = %self.identity.pubkey(),
            control = %config.listen,
            identify = ?config.identify,
            "node listening"
        );

        // Refusing every connection is the correct behaviour here and a baffling
        // one to debug, so say it once at startup rather than once per peer.
        if config.identify == Identify::Fips && config.listen.is_ipv4() {
            warn!(
                control = %config.listen,
                "listening on IPv4 while checking mesh identity: no peer on fips0 can reach this"
            );
        }

        tokio::spawn(wire::listen(control, wire_tx.clone(), config.identify));

        for peer in &config.peers {
            spawn_dialer(peer.clone(), wire_tx.clone(), config.identify);
        }

        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                Some(event) = wire_rx.recv() => self.on_wire(event, &done_tx).await,
                Some(event) = done_rx.recv() => self.dispatch(event, &done_tx).await,
                _ = ticker.tick() => {
                    self.on_tick(&done_tx).await;
                    self.publish(&config);
                }
                _ = &mut shutdown => break,
            }
        }

        self.wind_down(&done_tx).await;
        Ok(())
    }

    /// Say goodbye properly.
    ///
    /// A bare FIN reads to a peer as an unclean disconnect and starts the same
    /// cleanup a timeout would, so sending Disconnect first is the difference
    /// between the peer tearing our state down on a timer and doing it now.
    async fn wind_down(&mut self, done: &mpsc::Sender<Event>) {
        info!("shutting down: telling peers and settling");
        for action in self.sessions.shutdown() {
            self.execute(action, done).await;
        }

        // Give the outbound queues a moment to drain before the sockets go.
        // Nothing is lost if this expires — the peer falls back to its timeout.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    async fn on_wire(&mut self, event: Wire, done: &mpsc::Sender<Event>) {
        match event {
            Wire::PeerUp { peer, addr, tx } => {
                self.links.insert(peer, tx);
                // Tie the key the protocol knows to the address the kernel
                // knows, before anything is gated or shaped for this peer.
                self.adapter.register(peer, addr.ip());
                self.dispatch(Event::PeerConnected { peer }, done).await;
            }
            Wire::PeerDown { peer } => {
                self.links.remove(&peer);
                self.adapter.remove(peer);
                self.dispatch(Event::PeerDisconnected { peer }, done).await;
            }
            Wire::Message { peer, msg } => {
                // Core trusts what it is handed, so the signature is checked
                // here — before the message reaches anything that acts on it.
                // Every update in the purchase, since it is honored or refused
                // as a whole and one bad signature makes the whole thing
                // unauthentic.
                if let Message::TopUp(ref t) = msg
                    && !t.updates.iter().all(|u| {
                        self.channels
                            .verify_update(peer, u.channel_id, u.cumulative, u.signature)
                    })
                {
                    warn!(%peer, "discarding a TopUp whose signatures do not verify");
                    return;
                }
                if let Message::TopUpReject(ref r) = msg {
                    log_refusal(peer, r, Side::Received);
                }
                self.dispatch(Event::MessageReceived { peer, msg }, done)
                    .await;
            }
        }
    }

    /// Republish what the node is doing, for anything watching.
    fn publish(&self, config: &NodeConfig) {
        self.published.store(Arc::new(control::snapshot(
            &self.sessions,
            self.adapter.as_ref(),
            &hex::encode(self.identity.pubkey().0),
            &config.mint_url,
            self.started.elapsed().as_millis() as u64,
            self.now(),
        )));
    }

    /// Sample the meters and tick core.
    async fn on_tick(&mut self, done: &mpsc::Sender<Event>) {
        for peer in self.adapter.peers() {
            let counters = self.adapter.counters(peer);
            self.dispatch(Event::Metered { peer, counters }, done).await;

            let rate = self.adapter.demand(peer);
            self.dispatch(Event::DemandObserved { peer, rate }, done)
                .await;
        }
        self.dispatch(Event::Tick, done).await;
    }

    /// Feed one event to core and carry out everything it asks for.
    async fn dispatch(&mut self, event: Event, done: &mpsc::Sender<Event>) {
        let now = self.now();
        for action in self.sessions.handle(event, now) {
            self.execute(action, done).await;
        }
    }

    async fn execute(&mut self, action: Action, done: &mpsc::Sender<Event>) {
        match action {
            Action::Send { peer, msg } => {
                if let Message::TopUpReject(ref r) = msg {
                    log_refusal(peer, r, Side::Sent);
                }
                self.send(peer, msg).await
            }

            Action::SignAndSendTopUp {
                peer,
                ratchets,
                window_ms,
            } => {
                // One signature per channel, one message for the purchase: the
                // grant is their combined increase.
                let mut updates = Vec::with_capacity(ratchets.len());
                for (channel_id, cumulative) in ratchets {
                    match self.channels.sign_update(channel_id, cumulative) {
                        Ok(signature) => updates.push(ChannelUpdate {
                            channel_id,
                            cumulative,
                            signature,
                        }),
                        Err(e) => {
                            warn!(%peer, error = %e, "could not sign a channel update");
                            return;
                        }
                    }
                }
                self.send(peer, Message::TopUp(TopUp { updates, window_ms }))
                    .await;
            }

            Action::SetAccess { peer, access } => {
                debug!(%peer, ?access, "access changed");
                self.adapter.set_access(peer, access);
            }

            Action::SetShapingRate { peer, rate } => {
                debug!(%peer, rate, "shaping rate changed");
                self.adapter.set_shaping_rate(peer, rate);
            }

            Action::FundChannel {
                peer,
                mint_url,
                capacity,
            } => {
                let channels = Arc::clone(&self.channels);
                let done = done.clone();
                tokio::task::spawn_blocking(move || {
                    match channels.fund(peer, &mint_url, capacity) {
                        Ok(funded) => {
                            let _ = done.blocking_send(Event::OutgoingChannelFunded {
                                peer,
                                channel_id: funded.channel_id,
                                capacity: funded.capacity,
                                funding: funded.funding,
                            });
                        }
                        Err(e) => {
                            warn!(%peer, error = format!("{e:#}"), "could not fund a channel")
                        }
                    }
                });
            }

            Action::VerifyFunding { peer, funding } => {
                let channels = Arc::clone(&self.channels);
                let done = done.clone();
                tokio::task::spawn_blocking(move || {
                    let event = match channels.verify(peer, &funding) {
                        Ok(v) => Event::IncomingFundingVerified {
                            peer,
                            channel_id: v.channel_id,
                            capacity: v.capacity,
                        },
                        Err(e) => {
                            warn!(%peer, error = format!("{e:#}"), "peer funding did not verify");
                            Event::IncomingFundingRejected { peer }
                        }
                    };
                    let _ = done.blocking_send(event);
                });
            }

            Action::SettleChannel { peer, channel_id } => {
                // Worth logging: a channel settling far sooner than expected is
                // what a rollover going wrong looks like from outside.
                debug!(%peer, ?channel_id, "settling a channel");
                let channels = Arc::clone(&self.channels);
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = channels.settle(channel_id) {
                        warn!(%peer, error = format!("{e:#}"), "could not settle a channel");
                    }
                });
            }

            Action::DropPeer { peer } => {
                info!(%peer, "dropping peer");
                self.links.remove(&peer);
                self.adapter.remove(peer);
            }
        }
    }

    async fn send(&mut self, peer: PubKey, msg: Message) {
        let Some(link) = self.links.get(&peer) else {
            debug!(%peer, "no link for a message core wanted sent");
            return;
        };
        // A full outbox means the peer is not reading. Dropping is safe for the
        // only message that repeats: TopUp is cumulative, so the next one
        // carries the correct total anyway.
        if link.try_send(msg).is_err() {
            warn!(%peer, "outbox full, dropping a message");
        }
    }
}

/// Which end of a refusal we are.
#[derive(Debug, Clone, Copy)]
enum Side {
    /// We refused a peer's purchase.
    Sent,
    /// A peer refused ours.
    Received,
}

/// Record a refused purchase.
///
/// Both ends log it, because it means different things to each: the provider
/// learns a peer is asking for more than it will give, and the payer learns its
/// buying is being clipped and by how much. Neither number is otherwise
/// visible, so without this a peer sits silently pinned at a limit with nothing
/// to say why.
///
/// Severity follows [`ReasonCode::avoidable_from_offer`]. Nearly every reason
/// means the peer ignored something we advertised, or that a revised Offer
/// crossed its message in flight — a real fault, and a warning. A rate refusal
/// is not: the Offer carries no rate ceiling, so being refused and told what
/// would be accepted is how a payer is *supposed* to find the limit. Logging
/// that as a fault would bury the ones that are.
fn log_refusal(peer: PubKey, reject: &TopUpReject, side: Side) {
    let direction = match side {
        Side::Sent => "refused a peer's purchase",
        Side::Received => "a peer refused our purchase",
    };

    let channels = reject.refused.len();
    if reject.reason.avoidable_from_offer() {
        warn!(
            %peer,
            reason = ?reject.reason,
            channels,
            "{direction}: this should not happen against an Offer that was read"
        );
    } else {
        info!(
            %peer,
            channels,
            max_rate_available = reject.max_rate_available,
            "{direction}: rate above what is uncommitted"
        );
    }
}

/// Keep trying to reach a configured peer.
///
/// A peering is a standing relationship, not a one-shot connection, so a
/// refused dial is a reason to wait and try again rather than to give up.
///
/// A peer with no endpoint is one that dials us; there is nothing to reach out
/// to, and its policy is already in place for when it does.
fn spawn_dialer(peer: PeerConfig, wire_tx: mpsc::Sender<Wire>, identify: Identify) {
    let Some(endpoint) = peer.endpoint.clone() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            if let Err(e) = wire::dial(&endpoint, peer.pubkey, wire_tx.clone(), identify).await {
                debug!(%endpoint, error = %e, "control dial failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
