//! Two real nodes, two real sockets, real bytes.
//!
//! The in-memory harness in `tollgate-core` proves the state machine is right.
//! This proves the whole thing works when the clock, the sockets and the signer
//! are real: traffic appears, the buyer notices, it signs a TopUp, the provider
//! admits it, the shaper opens up, and the throughput the operator sees follows.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_net::adapter::{Loopback, ResourceAdapter};
use tollgate_net::channel::LocalChannels;
use tollgate_net::identity::Identity;
use tollgate_net::node::{Node, NodeConfig, PeerConfig};
use tollgate_net::wire::Identify;
use tollgate_protocol::PubKey;

/// Each node needs two consecutive ports, and tests share a process, so hand
/// out non-overlapping blocks rather than hoping.
static NEXT_PORT: AtomicU16 = AtomicU16::new(24_700);

fn take_port_pair() -> u16 {
    NEXT_PORT.fetch_add(2, Ordering::Relaxed)
}

fn node_policy(mint: &str) -> NodePolicy {
    NodePolicy {
        unit: "byte".into(),
        accepted_mints: vec![mint.into()],
        received_multiplier: 0,
        minimum_flow: 4_096,
        grants: GrantPolicy {
            min_window_ms: 200,
            max_window_ms: 30_000,
            max_rate: None,
        },
        initial_channel_capacity: 10_000_000_000,
        stale_timeout_ms: 0,
        rollover_threshold_pct: 80,
    }
}

fn buyer_policy() -> BuyerPolicy {
    BuyerPolicy {
        // A short window keeps the demo responsive: the forfeit on raising the
        // rate can never exceed one window's worth.
        window_ms: 1_000,
        renew_lead_ms: 300,
        ..BuyerPolicy::default()
    }
}

/// Start a node and return its identity and adapter.
fn spawn_node(port: u16, mint: &str, peers: Vec<PeerConfig>) -> (PubKey, Arc<Loopback>) {
    let (pubkey, adapter, _) = spawn_node_as(Identity::generate(), port, mint, peers);
    (pubkey, adapter)
}

/// The same, for a test that has to know a node's key before it starts — a
/// policy written about a peer names it, so somebody has to be first.
///
/// Also hands back what the node publishes, which is where a session either
/// appears or does not.
fn spawn_node_as(
    identity: Identity,
    port: u16,
    mint: &str,
    peers: Vec<PeerConfig>,
) -> (PubKey, Arc<Loopback>, tollgate_net::control::Published) {
    let pubkey = identity.pubkey();
    let listen: SocketAddr = format!("127.0.0.1:{port}").parse().expect("address");

    let config = NodeConfig {
        identity,
        policy: node_policy(mint),
        buyer: buyer_policy(),
        listen,
        // These tests talk plain IP on loopback, where an address commits to
        // nothing there is to check.
        identify: Identify::Claimed,
        // Unused here: these tests drive `LocalChannels`, so no mint is served.
        mint_listen: format!("127.0.0.1:{}", port + 10_000)
            .parse()
            .expect("address"),
        mint_url: format!("http://127.0.0.1:{}", port + 10_000),
        peers,
    };

    let adapter = Arc::new(Loopback::new());
    let node = Node::new(
        &config,
        Arc::new(LocalChannels::new(config.identity.clone())),
        adapter.clone(),
    );
    let published = node.published();
    spawn_loopback_plane(&config, adapter.clone());
    tokio::spawn(async move {
        if let Err(e) = node.run(config, std::future::pending()).await {
            eprintln!("node stopped: {e}");
        }
    });

    (pubkey, adapter, published)
}

/// Start the loopback data plane for a node: a listener, and a dialer per peer.
///
/// The node does not do this itself, because a kernel adapter forwards real
/// traffic and has no socket of its own.
fn spawn_loopback_plane(config: &NodeConfig, adapter: Arc<Loopback>) {
    let listen = config.data_listen();
    let local = config.identity.pubkey();
    let a = adapter.clone();
    tokio::spawn(async move {
        if let Ok(l) = tokio::net::TcpListener::bind(listen).await {
            let _ = tollgate_net::dataplane::listen(l, a).await;
        }
    });
    for peer in &config.peers {
        let Some(endpoint) = peer.endpoint.clone() else {
            continue;
        };
        tollgate_net::dataplane::keep_dialing(endpoint, local, peer.pubkey, adapter.clone());
    }
}

/// Poll until `check` passes, or give up. Real sockets and a real clock mean
/// nothing is instant; a deadline beats a fixed sleep.
async fn wait_for(label: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {label}");
}

/// Bytes per second actually arriving from a peer, measured over `window`.
async fn measure_download(adapter: &Loopback, peer: PubKey, window: Duration) -> u64 {
    let before = adapter.counters(peer).received;
    tokio::time::sleep(window).await;
    let after = adapter.counters(peer).received;
    (after - before) * 1_000 / window.as_millis() as u64
}

/// Bring up a provider and a client, connected and paying.
async fn connected_pair() -> (PubKey, Arc<Loopback>, PubKey, Arc<Loopback>) {
    let provider_port = take_port_pair();
    let client_port = take_port_pair();

    let (provider, provider_adapter) =
        spawn_node(provider_port, "https://provider.example/mint", vec![]);

    // The client dials, so it is the side that has to know the provider's key
    // up front. A listener learns who is calling from the Announce.
    let (client, client_adapter) = spawn_node(
        client_port,
        "https://client.example/mint",
        vec![PeerConfig {
            pubkey: provider,
            endpoint: Some(format!("127.0.0.1:{provider_port}")),
            policy: PeerPolicy::default(),
        }],
    );

    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the provider to see the client",
        Duration::from_secs(10),
        move || probe.peers().contains(&client),
    )
    .await;

    (provider, provider_adapter, client, client_adapter)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_wants_bandwidth_buys_it_and_gets_it() {
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    // Nothing bought yet: the client is held at the minimum flow allowance,
    // which is what leaves it able to send the TopUp that changes that.
    assert!(
        provider_adapter.shaping_rate(client) <= 4_096,
        "an unpaid peer should be at the allowance, not above it"
    );

    // Traffic appears.
    const WANTED: u64 = 2_000_000;
    client_adapter.set_demand(provider, WANTED);

    // 125% headroom on 2 MB/s.
    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the provider to shape the client to what it bought",
        Duration::from_secs(10),
        move || probe.shaping_rate(client) == 2_500_000,
    )
    .await;

    let throughput = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;
    assert!(
        throughput > WANTED,
        "the client bought 2.5 MB/s and measured only {throughput} B/s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traffic_spike_raises_the_purchased_rate() {
    // The point of the whole exercise: the algorithm notices demand climbing
    // and buys more, without anything being negotiated or acknowledged.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 1_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the first purchase", Duration::from_secs(10), move || {
        probe.shaping_rate(client) == 1_250_000
    })
    .await;

    let before = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;

    // Demand jumps sixteenfold.
    client_adapter.set_demand(provider, 16_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the rate to be raised",
        Duration::from_secs(10),
        move || probe.shaping_rate(client) == 20_000_000,
    )
    .await;

    let after = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;
    assert!(
        after > before * 4,
        "throughput should have followed the purchase: {before} B/s -> {after} B/s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_buying_falls_back_to_the_allowance() {
    // Non-payment enforces itself. Nothing detects it, no message announces it,
    // and there is no delivered-but-unpaid balance to chase.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 4_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the purchase", Duration::from_secs(10), move || {
        probe.shaping_rate(client) == 5_000_000
    })
    .await;

    client_adapter.set_demand(provider, 0);

    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the grant to lapse back to the allowance",
        Duration::from_secs(10),
        move || probe.shaping_rate(client) == 4_096,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_directions_are_bought_and_shaped_independently() {
    // Two channels is the default. Each side pays for what it received, so both
    // owe, both fund, and each buys on its own schedule.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 1_000_000);
    provider_adapter.set_demand(client, 4_000_000);

    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the client's purchase",
        Duration::from_secs(10),
        move || probe.shaping_rate(client) == 1_250_000,
    )
    .await;

    let probe = Arc::clone(&client_adapter);
    wait_for(
        "the provider's purchase",
        Duration::from_secs(10),
        move || probe.shaping_rate(provider) == 5_000_000,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_that_fills_up_rolls_over_and_buying_continues() {
    // A channel sized to be exhausted in a few seconds, so the boundary is
    // crossed several times inside the test. Before per-channel cumulative,
    // this looped funding a channel per tick and then stopped buying forever.
    let provider_port = take_port_pair();
    let client_port = take_port_pair();

    let small_channels = NodePolicy {
        // ~2 seconds of traffic at the rate the client will buy.
        initial_channel_capacity: 5_000_000,
        ..node_policy("https://provider.example/mint")
    };

    let identity = Identity::generate();
    let provider = identity.pubkey();
    let config = NodeConfig {
        identity,
        policy: small_channels.clone(),
        buyer: buyer_policy(),
        listen: format!("127.0.0.1:{provider_port}")
            .parse()
            .expect("address"),
        identify: Identify::Claimed,
        // Unused here: these tests drive `LocalChannels`, so no mint is served.
        mint_listen: format!("127.0.0.1:{}", provider_port + 10_000)
            .parse()
            .expect("address"),
        mint_url: format!("http://127.0.0.1:{}", provider_port + 10_000),
        peers: vec![],
    };
    let provider_adapter = Arc::new(Loopback::new());
    let node = Node::new(
        &config,
        Arc::new(LocalChannels::new(config.identity.clone())),
        provider_adapter.clone(),
    );
    spawn_loopback_plane(&config, provider_adapter.clone());
    tokio::spawn(async move {
        let _ = node.run(config, std::future::pending::<()>()).await;
    });

    let (client, client_adapter) = spawn_node(
        client_port,
        "https://client.example/mint",
        vec![PeerConfig {
            pubkey: provider,
            endpoint: Some(format!("127.0.0.1:{provider_port}")),
            policy: PeerPolicy::default(),
        }],
    );

    let probe = Arc::clone(&provider_adapter);
    wait_for("the peering", Duration::from_secs(10), move || {
        probe.peers().contains(&client)
    })
    .await;

    client_adapter.set_demand(provider, 2_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the first purchase", Duration::from_secs(10), move || {
        probe.shaping_rate(client) == 2_500_000
    })
    .await;

    // Long enough to run through several channels' worth of capacity.
    let sustained = measure_download(&client_adapter, provider, Duration::from_secs(8)).await;
    assert!(
        sustained > 1_500_000,
        "throughput collapsed after a rollover: {sustained} B/s"
    );

    // And it is still shaping at what was bought, not at the allowance.
    assert_eq!(
        provider_adapter.shaping_rate(client),
        2_500_000,
        "the client stopped being able to buy"
    );
}

/// A peer the operator refused is refused when it calls in.
///
/// The refusal has to survive the fact that there is nothing to dial: a node
/// does not reach out to a peer it will not talk to, so the entry carrying
/// `blocked` is exactly the entry with no endpoint. Losing such entries for
/// want of an address would admit precisely the peer being turned away.
#[tokio::test]
async fn a_blocked_peer_that_dials_in_is_refused() {
    let provider_port = take_port_pair();
    let client_port = take_port_pair();

    // Somebody has to be named first, and it is the side being refused.
    let client_identity = Identity::generate();
    let client = client_identity.pubkey();

    let (provider, _provider_adapter, published) = spawn_node_as(
        Identity::generate(),
        provider_port,
        "https://provider.example/mint",
        vec![PeerConfig {
            pubkey: client,
            endpoint: None,
            policy: PeerPolicy {
                blocked: true,
                ..PeerPolicy::default()
            },
        }],
    );

    let (_client, _client_adapter, _) = spawn_node_as(
        client_identity,
        client_port,
        "https://client.example/mint",
        vec![PeerConfig {
            pubkey: provider,
            endpoint: Some(format!("127.0.0.1:{provider_port}")),
            policy: PeerPolicy::default(),
        }],
    );

    // The client redials every couple of seconds, so this covers several
    // attempts rather than catching the gap between two of them.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let peers = &published.load().peers;
    assert!(
        peers.is_empty(),
        "the blocked peer opened a session anyway: {peers:?}"
    );
}
