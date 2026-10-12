//! The TollGate daemon.
//!
//! Reads a YAML configuration, brings up the control and data planes, and runs
//! until interrupted. `--demand` drives the traffic generator, which is what
//! makes the buying algorithm do anything visible.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tollgate_net::channel::{SpilmanChannels, SpilmanConfig};
use tollgate_net::config::{File, ForwardingMode};
use tollgate_net::mint::{self, MintConfig};
use tollgate_net::node::Node;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "tollgated", about = "A TollGate node")]
struct Args {
    /// Configuration file. Defaults are used if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print the identity that would be used, then exit.
    ///
    /// Peers are dialed by public key, so bringing up a pair means knowing each
    /// other's keys before either starts.
    #[arg(long)]
    show_identity: bool,

    /// Traffic to generate towards every peer, in bytes per second.
    ///
    /// This is the demand the buying algorithm reacts to.
    #[arg(long, default_value_t = 0)]
    demand: u64,

    /// Step demand up by this many bytes per second every `--ramp-interval`,
    /// so a demo can show the purchased rate climbing to meet it.
    #[arg(long, default_value_t = 0)]
    ramp: u64,

    /// How often to apply `--ramp`.
    #[arg(long, default_value_t = 5)]
    ramp_interval: u64,

    /// Report throughput and what has been bought, this often, in seconds.
    /// Zero turns the report off.
    #[arg(long, default_value_t = 1)]
    report: u64,

    /// Where to serve the control socket that `tolltop` reads.
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

/// Derive the wallet's seed from the node's identity.
///
/// Same reasoning as the mint's, and a different domain string so the two are
/// unrelated: a wallet derived from the node's key is restored by restoring the
/// config, rather than being a second thing to back up.
fn wallet_seed(secret_hex: &str) -> Result<[u8; 64]> {
    use sha2::{Digest, Sha512};
    let secret = hex::decode(secret_hex).context("identity key is not hex")?;
    let mut hasher = Sha512::new();
    hasher.update(b"tollgate-wallet-seed");
    hasher.update(&secret);
    Ok(hasher.finalize().into())
}

/// Derive the mint's keyset seed from the node's identity.
///
/// Deterministic, so a restart keeps issuing against the same keys and the
/// vouchers a peer already holds stay redeemable. Hashed rather than used
/// directly so the mint's key material is not the node's signing key.
fn mint_seed(secret_hex: &str) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let secret = hex::decode(secret_hex).context("identity key is not hex")?;
    let mut hasher = Sha256::new();
    hasher.update(b"tollgate-mint-seed");
    hasher.update(&secret);
    Ok(hasher.finalize().to_vec())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // Colour only for a human at a terminal. Redirected to a file or a
        // pipe, escape codes land in the middle of every field and make the
        // output unreadable to anything that tries to parse it.
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let args = Args::parse();

    let file = match &args.config {
        Some(path) => File::load(path)?,
        None => serde_yaml::from_str("{}").expect("the empty document is valid"),
    };
    let config = file.resolve().context("resolve configuration")?;

    if args.show_identity {
        println!("pubkey:     {}", hex::encode(config.identity.pubkey().0));
        println!("secret_key: {}", config.identity.secret_hex());
        return Ok(());
    }

    info!(
        pubkey = %hex::encode(config.identity.pubkey().0),
        "starting"
    );

    // What this node takes as payment, and at what price. Held here because
    // three things share it: the market applies it, the control socket changes
    // it while the node runs, and the display shows it.
    let prices = tollgate_net::market::Prices::new(file.market.accepted());

    // What it holds. The market pays into it, channels are funded out of it,
    // and it is derived from the node's own secret so that restoring a config
    // restores the balance with it.
    let wallet_path = if file.wallet.file.is_empty() {
        tollgate_net::wallet::default_path()
    } else {
        file.wallet.file.clone().into()
    };
    let wallet = tollgate_net::wallet::Wallet::open(
        &wallet_path,
        wallet_seed(&config.identity.secret_hex())?,
        // What this node sells, so a holding denominated in it can be told
        // apart from money: it is capacity bought from an upstream.
        config.policy.unit.clone(),
    )
    .await
    .context("open this node's wallet")?;
    info!(wallet = %wallet_path.display(), "holding");

    // Whatever the last run left half-done, finished or rolled back, and
    // anything already paid for collected. A node killed mid-mint leaves its
    // quote reserved, and the money behind it is only claimable once that is
    // settled — so this is a startup step rather than something to discover
    // later. It talks to a mint, so it happens off the critical path and its
    // failure is a log line, not a refusal to start.
    if !file.wallet.mint.is_empty() {
        let wallet = wallet.clone();
        let (mint, unit) = (file.wallet.mint.clone(), file.wallet.unit.clone());
        tokio::spawn(async move {
            match wallet.claim_pending(&mint, &unit).await {
                Ok(0) => {}
                Ok(claimed) => info!(claimed, %unit, "collected what was already paid for"),
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "nothing collectable at startup")
                }
            }
        });
    }

    // The mint comes up first. A peer funds its channel against *our* mint, so
    // nothing can be paid for until it is serving.
    let mint = Arc::new(
        mint::build(&MintConfig {
            url: config.mint_url.clone(),
            unit: config.policy.unit.clone(),
            // Derived from the identity so a restart keeps issuing against the
            // same keys, and two nodes never share a keyset.
            seed: mint_seed(&config.identity.secret_hex())?,
            max_amount: config.policy.initial_channel_capacity.max(1),
        })
        .await
        .context("bring up this node's mint")?,
    );

    {
        let market = tollgate_net::market::router(
            Arc::clone(&mint),
            config.policy.unit.clone(),
            config.mint_url.clone(),
            prices.clone(),
            wallet.clone(),
            config.policy.initial_channel_capacity.max(1),
        );
        let mint = Arc::clone(&mint);
        let listen = config.mint_listen;
        tokio::spawn(async move {
            if let Err(e) = mint::serve(mint, market, listen, std::future::pending()).await {
                tracing::error!(error = %e, "the mint stopped");
            }
        });
    }
    info!(
        url = %config.mint_url,
        listen = %config.mint_listen,
        unit = %config.policy.unit,
        "mint serving"
    );

    for accepted in prices.listed() {
        info!(
            mint = %accepted.mint,
            unit = %accepted.unit,
            bytes_per_unit = accepted.bytes_per_unit,
            "taking payment in"
        );
    }
    if !prices.is_selling() {
        tracing::warn!("this node takes no paper as payment, so nobody can buy its vouchers here");
    }

    // A byte source on the mesh. Off unless asked for: it is an instrument, and
    // an unauthenticated one, so a node that was never told to serve it should
    // not be.
    if file.speedtest.enabled {
        let (listen, settings) = file
            .speedtest
            .resolve()
            .context("resolve the speedtest configuration")?;
        tokio::spawn(async move {
            if let Err(e) =
                tollgate_net::speedtest::serve(settings, listen, std::future::pending()).await
            {
                tracing::error!(error = %format!("{e:#}"), "the speedtest stopped");
            }
        });
        info!(
            %listen,
            streams = file.speedtest.streams,
            "speedtest serving"
        );
    }

    let channels = Arc::new(
        SpilmanChannels::new(SpilmanConfig {
            mint: Arc::clone(&mint),
            mint_url: config.mint_url.clone(),
            unit: config.policy.unit.clone(),
            accepted_mints: config.policy.accepted_mints.clone(),
            secret_key_hex: config.identity.secret_hex(),
            wallet: wallet.clone(),
            // A node with no money mint configured can still sell; it just
            // cannot buy, and says so when something asks it to.
            money: (!file.wallet.mint.is_empty()).then(|| tollgate_net::channel::Money {
                mint: file.wallet.mint.clone(),
                unit: file.wallet.unit.clone(),
            }),
        })
        .context("build the channel backend")?,
    );

    // Which thing actually delivers. The loopback shaper carries a socket of
    // its own and forwards nobody's traffic; the kernel adapter gates and
    // shapes the real forwarding path.
    let adapter: Arc<dyn tollgate_net::adapter::ResourceAdapter> = match file.forwarding.mode {
        ForwardingMode::Loopback => {
            let loopback = Arc::new(tollgate_net::adapter::Loopback::new());

            // The loopback data plane belongs to the loopback adapter, so it is
            // started here rather than by the node.
            let data = tokio::net::TcpListener::bind(config.data_listen())
                .await
                .with_context(|| format!("bind the data plane on {}", config.data_listen()))?;
            tokio::spawn(tollgate_net::dataplane::listen(data, loopback.clone()));
            for peer in &config.peers {
                // Only toward a peer we dial. One that dials us brings its own
                // data-plane connection with it.
                if let Some(endpoint) = &peer.endpoint {
                    tollgate_net::dataplane::keep_dialing(
                        endpoint.clone(),
                        config.identity.pubkey(),
                        peer.pubkey,
                        loopback.clone(),
                    );
                }
            }
            info!(listen = %config.data_listen(), "loopback data plane");
            loopback
        }
        #[cfg(target_os = "linux")]
        ForwardingMode::Nftables => {
            let interface = file.forwarding.interface.clone();
            let adapter = tollgate_net::adapter::Nftables::new(&interface).with_context(|| {
                format!("set up nftables and tc on {interface}; CAP_NET_ADMIN is required")
            })?;
            info!(%interface, "gating and shaping the kernel forwarding path");
            Arc::new(adapter)
        }
        #[cfg(not(target_os = "linux"))]
        ForwardingMode::Nftables => {
            anyhow::bail!("forwarding.mode: nftables needs Linux")
        }
        #[cfg(unix)]
        ForwardingMode::Fips => {
            let socket = if file.forwarding.fips_socket.is_empty() {
                tollgate_net::adapter::Fips::default_socket_path()
            } else {
                file.forwarding.fips_socket.clone().into()
            };
            let adapter = tollgate_net::adapter::Fips::new(&socket, config.policy.minimum_flow)
                .with_context(|| {
                    format!(
                        "reach the FIPS control socket at {}; is fipsd running?",
                        socket.display()
                    )
                })?;
            info!(
                socket = %socket.display(),
                allowance = config.policy.minimum_flow,
                "setting transit policy on the FIPS node"
            );
            Arc::new(adapter)
        }
        #[cfg(not(unix))]
        ForwardingMode::Fips => {
            anyhow::bail!("forwarding.mode: fips needs a Unix control socket")
        }
    };

    let node = Node::new(&config, channels, adapter.clone());

    // Anything watching this node reads here. A Unix socket rather than a port:
    // it is operational state for a local tool, not something a peer acts on.
    {
        let published = node.published();
        let path = args
            .control_socket
            .clone()
            .unwrap_or_else(tollgate_net::control::default_socket_path);
        let prices = prices.clone();
        let wallet = wallet.clone();
        let money = (!file.wallet.mint.is_empty()).then(|| tollgate_net::channel::Money {
            mint: file.wallet.mint.clone(),
            unit: file.wallet.unit.clone(),
        });
        tokio::spawn(async move {
            if let Err(e) = tollgate_net::control::serve(
                &path,
                published,
                prices,
                wallet,
                money,
                std::future::pending(),
            )
            .await
            {
                tracing::warn!(error = %format!("{e:#}"), "the control socket stopped");
            }
        });
    }

    // Demand is what we want to pull from a peer, which is what the buyer
    // reacts to; the shaper decides how much of it arrives.
    //
    // The flag wins over the file so that a run can be told to want something
    // else without editing what the service starts with — but the file is where
    // a node that should keep a link paid for says so, because the alternative
    // is editing a launchd plist or an init script to buy anything at all.
    let demand = if args.demand > 0 {
        args.demand
    } else {
        file.buying.demand
    };
    if demand > 0 || args.ramp > 0 {
        info!(demand, "wanting");
        let adapter = Arc::clone(&adapter);
        let (base, ramp, interval) = (demand, args.ramp, args.ramp_interval.max(1));
        tokio::spawn(async move {
            let mut steps = 0u64;
            loop {
                let demand = base.saturating_add(ramp.saturating_mul(steps));
                for peer in adapter.peers() {
                    adapter.set_demand(peer, demand);
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if ramp > 0 {
                    steps += 1;
                }
            }
        });
    }

    if args.report > 0 {
        let adapter = Arc::clone(&adapter);
        let period = Duration::from_secs(args.report);
        tokio::spawn(async move {
            let mut last: std::collections::HashMap<_, (u64, u64)> = Default::default();
            loop {
                tokio::time::sleep(period).await;
                for peer in adapter.peers() {
                    let now = adapter.counters(peer);
                    let (was_delivered, was_received) = last
                        .insert(peer, (now.delivered, now.received))
                        .unwrap_or((0, 0));

                    let secs = period.as_secs().max(1);
                    info!(
                        peer = %peer,
                        shaped = adapter.shaping_rate(peer),
                        demand = adapter.demand(peer),
                        down = (now.received - was_received) / secs,
                        up = (now.delivered - was_delivered) / secs,
                        "link"
                    );
                }
            }
        });
    }

    node.run(config, interrupted()).await
}

/// Resolves on the first interrupt or termination signal.
///
/// SIGTERM as well as Ctrl-C, because a container is stopped with the former
/// and a node that ignored it would be killed mid-session.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "no SIGTERM handler; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
