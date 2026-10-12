//! What the node will tell a local tool about itself, and the one thing it
//! will take instructions about.
//!
//! A Unix socket serving a JSON snapshot, one per connection. The node
//! republishes the snapshot on every tick and the socket hands out whatever is
//! current, so a reader never blocks the event loop and the loop never waits on
//! a reader.
//!
//! A caller that says nothing gets the snapshot, which is what every reader
//! did before there was anything to say. A caller that sends a line of JSON
//! first gets an answer to it instead — see [`Request`]. The only thing that
//! can be changed this way is the market price: it is the one number an
//! operator has a reason to move while the node runs, and moving it does not
//! touch a session, a grant or a channel.
//!
//! Local-only by construction: a Unix socket has no port to expose, and
//! nothing here is reachable by a peer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tollgate_core::access::AccessLevel;
use tollgate_core::session::{Phase, Sessions};
use tracing::debug;

use crate::adapter::ResourceAdapter;
use crate::market::{Accepted, Prices};
use crate::wallet::{Holding, TopUp, Wallet};

/// Where a control socket might live, best first.
///
/// Four places, for four ways of running a node. `/run` is where a Linux
/// service manager puts it — systemd's `RuntimeDirectory=`, and the path the
/// container images and the OpenWrt package use. `/usr/local/var/run` is the
/// macOS package's equivalent, since `/run` there is neither writable nor
/// something launchd populates. `XDG_RUNTIME_DIR` is where a node run by a
/// person on their own machine belongs, and `/tmp` is the fallback that exists
/// everywhere.
///
/// The daemon creates the first of these it can, and a reader looks for the
/// first that is already there. That asymmetry is the point: a tool should find
/// a running node without being told where it put its socket.
fn socket_candidates() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/run/tollgate.sock"),
        PathBuf::from("/usr/local/var/run/tollgate.sock"),
    ];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        paths.push(PathBuf::from(format!("{xdg}/tollgate.sock")));
    }
    paths.push(PathBuf::from("/tmp/tollgated.sock"));
    paths
}

/// Where this node should put its control socket unless told otherwise.
///
/// The first candidate whose directory we can actually write to. A daemon that
/// picked a path it could not create would fail at startup over something an
/// operator never asked for.
pub fn default_socket_path() -> PathBuf {
    for path in socket_candidates() {
        let Some(dir) = path.parent() else {
            continue;
        };
        // Writability is asked of the directory rather than assumed from the
        // user id: a container runs as root and a laptop does not, and both are
        // ordinary ways to run this.
        if dir
            .metadata()
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
            && std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(".tollgate-write-test"))
                .map(|_| {
                    let _ = std::fs::remove_file(dir.join(".tollgate-write-test"));
                })
                .is_ok()
        {
            return path;
        }
    }
    PathBuf::from("/tmp/tollgated.sock")
}

/// Where a running node's control socket already is.
///
/// Returns the first candidate that exists, so `tolltop` with no arguments
/// finds a node started with no arguments — and finds one inside a container,
/// where the socket is in `/run` because that is where a service manager puts
/// it.
pub fn find_socket() -> Result<PathBuf> {
    let candidates = socket_candidates();
    for path in &candidates {
        // Existence, not connectability: a socket that is there but not
        // answering is a node that is starting or has just died, and saying so
        // is more useful than moving on to a stale path somewhere else.
        if path.exists() {
            return Ok(path.clone());
        }
    }

    let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    anyhow::bail!(
        "no control socket found; looked in {}. Is a node running, and did it \
         put its socket somewhere else? Pass --socket if so.",
        looked.join(", ")
    )
}

/// Everything one node is currently doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// This node's public key, hex-encoded.
    pub pubkey: String,
    /// The unit it denominates in.
    pub unit: String,
    /// The mint it advertises.
    pub mint_url: String,
    /// How long it has been up, in milliseconds.
    pub uptime_ms: u64,
    /// One entry per peer, ordered by key so the display does not jump around.
    pub peers: Vec<PeerSnapshot>,
    /// What this node takes as payment, and at what price.
    ///
    /// Filled in where the snapshot is served rather than where it is built:
    /// prices belong to the market, and the node's own state machine has no
    /// opinion about money. A reader wants both in one answer all the same.
    #[serde(default)]
    pub accepts: Vec<Accepted>,
    /// What it is holding, money first.
    #[serde(default)]
    pub holdings: Vec<Holding>,
    /// Where this node buys the money it pays with, if it buys at all.
    ///
    /// Held separately from `holdings` because a node that has spent its
    /// balance to nothing still has a mint it buys at — and that is exactly
    /// when somebody wants to top it up.
    #[serde(default)]
    pub money_mint: Option<Accepted>,
}

/// One peering, from this node's side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSnapshot {
    /// Their public key, hex-encoded.
    pub pubkey: String,
    /// Where they are in the opening sequence.
    pub phase: String,
    /// What we will deliver for them.
    pub access: String,

    // --- what they bought from us -----------------------------------------
    /// Units per second we are letting them draw. Their grant, or the minimum
    /// flow allowance if it has lapsed.
    pub shaped_rate: u64,
    /// Cumulative units they have paid for, across every channel.
    pub authorized: u64,
    /// Cumulative units drawn against them.
    pub consumed: u64,
    /// Milliseconds until the grant in force lapses. Zero if none is live.
    pub grant_expires_in_ms: u64,
    /// Channels they pay us on, and how full each is.
    pub incoming_channels: Vec<ChannelSnapshot>,

    // --- what we bought from them -----------------------------------------
    /// Units per second we last bought.
    pub bought_rate: u64,
    /// Units per second we want from them.
    pub demand: u64,
    /// Units per second we are pushing at them.
    pub upload_rate: u64,
    /// Their surcharge on what we push at them.
    pub received_multiplier: u16,
    /// The channel we are paying on, if one is open.
    pub outgoing_channel: Option<ChannelSnapshot>,
    /// Whether a replacement is funded and waiting behind it.
    pub rollover_ready: bool,

    // --- what actually moved ----------------------------------------------
    /// Cumulative units delivered to them.
    pub delivered: u64,
    /// Cumulative units received from them.
    pub received: u64,
}

/// One channel, and how much of it is spent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSnapshot {
    /// Its identifier, hex-encoded and shortened.
    pub id: String,
    /// Units it can carry in total.
    pub capacity: u64,
    /// Units signed onto it so far.
    pub signed: u64,
}

/// The latest snapshot, republished by the node and read by the socket.
pub type Published = Arc<ArcSwap<Snapshot>>;

/// Build a snapshot of everything the node is doing right now.
pub fn snapshot(
    sessions: &Sessions,
    adapter: &dyn ResourceAdapter,
    pubkey: &str,
    mint_url: &str,
    uptime_ms: u64,
    now: tollgate_core::Millis,
) -> Snapshot {
    let policy = sessions.node_policy();

    let peers = sessions
        .peers()
        .map(|session| {
            let counters = adapter.counters(session.peer);
            let grant = &session.grant;

            PeerSnapshot {
                pubkey: hex::encode(session.peer.0),
                phase: match session.phase {
                    Phase::Opening => "opening",
                    Phase::Establishing => "establishing",
                    Phase::Established => "established",
                    Phase::Closing => "closing",
                }
                .into(),
                access: match session.access {
                    AccessLevel::None => "none",
                    AccessLevel::Active => "active",
                    AccessLevel::Free => "free",
                    AccessLevel::Suspended => "suspended",
                }
                .into(),

                shaped_rate: adapter.shaping_rate(session.peer),
                authorized: grant.authorized(),
                consumed: grant.consumed(),
                grant_expires_in_ms: if grant.is_live(now) {
                    grant.deadline().saturating_since(now)
                } else {
                    0
                },
                incoming_channels: grant
                    .channels()
                    .iter()
                    .map(|c| ChannelSnapshot {
                        id: short(&c.id.0),
                        capacity: c.capacity,
                        signed: c.signed,
                    })
                    .collect(),

                bought_rate: session.buyer.rate(),
                demand: session.demand,
                upload_rate: session.upload_rate,
                received_multiplier: session
                    .offer
                    .as_ref()
                    .map(|o| o.received_multiplier)
                    .unwrap_or(0),
                outgoing_channel: session.buyer.active().map(|c| ChannelSnapshot {
                    id: short(&c.id.0),
                    capacity: c.capacity,
                    signed: c.cumulative,
                }),
                rollover_ready: session.buyer.next_channel().is_some(),

                delivered: counters.delivered,
                received: counters.received,
            }
        })
        .collect();

    Snapshot {
        pubkey: pubkey.into(),
        unit: policy.unit.clone(),
        mint_url: mint_url.into(),
        uptime_ms,
        // Filled in when the snapshot is served: what the node charges and what
        // it holds are the market's business and the wallet's, not the session
        // layer's.
        accepts: Vec::new(),
        holdings: Vec::new(),
        money_mint: None,
        peers,
    }
}

/// First four bytes in hex — enough to correlate, short enough to read.
fn short(bytes: &[u8]) -> String {
    hex::encode(&bytes[..4.min(bytes.len())])
}

/// How long to wait for a request before assuming there is not going to be one.
///
/// Short: a caller with something to say has already written it by the time the
/// connection is accepted. This is only the ceiling on how long a caller that
/// wants the snapshot and says so by staying quiet has to wait for it.
const REQUEST_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

/// A line of JSON asking the node for something.
///
/// Shaped like the FIPS control socket's — a command and its parameters — so
/// an operator driving both is not learning two conventions.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Everything the node is doing. The same thing a caller gets by saying
    /// nothing at all.
    Snapshot,
    /// What the node takes as payment, and at what price.
    ShowPrices,
    /// Change what one issuer's paper buys here.
    ///
    /// Per issuer, because that is what a price is about: paper from a mint an
    /// operator has stopped trusting is worth less, and zero stops taking it.
    SetPrice {
        /// The mint whose paper this is about.
        mint: String,
        /// Its unit. Defaults to `sat`, as everywhere else.
        #[serde(default = "default_unit")]
        unit: String,
        /// Units of capacity one unit of that paper buys. Zero refuses it.
        bytes_per_unit: u64,
    },
    /// What this node is holding.
    ShowWallet,
    /// Claim any top-up that has been paid for but not collected.
    ///
    /// The node collects a top-up by itself while it is waiting on one, so this
    /// is for when that went wrong: the node restarted mid-wait, or the mint
    /// was refusing the status polls long enough for the waiter to give up. The
    /// quote is in the wallet's database either way.
    Claim {
        /// Which mint to claim at. Defaults to this node's money mint.
        #[serde(default)]
        mint: Option<String>,
        /// In what. Defaults to `sat`.
        #[serde(default = "default_unit")]
        unit: String,
    },
    /// Buy money, and hand back the invoice that pays for it.
    ///
    /// The node claims what it bought as soon as the invoice is paid, so the
    /// caller's job is to pay it and watch the balance — there is nothing to
    /// confirm afterwards.
    TopUp {
        /// How much to buy.
        amount: u64,
        /// Where to buy it. Defaults to the mint this node already holds its
        /// money at, which is the answer nearly every time.
        #[serde(default)]
        mint: Option<String>,
        /// In what. Defaults to `sat`.
        #[serde(default = "default_unit")]
        unit: String,
    },
}

fn default_unit() -> String {
    "sat".into()
}

/// What came back.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// The command was carried out.
    Ok {
        /// Whatever it produced.
        data: serde_json::Value,
    },
    /// It was not.
    Error {
        /// Why.
        message: String,
    },
}

/// Serve the snapshot, and the few commands, until `shutdown`.
///
/// One exchange per connection, then close: a tool that wants live data
/// reconnects, which keeps this side stateless and means a stalled reader can
/// never hold anything up.
pub async fn serve(
    path: &Path,
    published: Published,
    prices: Prices,
    wallet: Wallet,
    money: Option<crate::channel::Money>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    // A socket left behind by a previous run would make bind fail. Removing it
    // is safe because binding is what proves nobody else is listening.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind the control socket at {}", path.display()))?;

    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        debug!(error = %e, "control connection failed to accept");
                        continue;
                    }
                };
                let published = Arc::clone(&published);
                let prices = prices.clone();
                let wallet = wallet.clone();
                let money = money.clone();
                tokio::spawn(async move {
                    let _ = answer(stream, published, prices, wallet, money).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Read at most one request and write exactly one answer.
///
/// The request is optional: a caller that closes its writing half without
/// sending anything is asking for the snapshot, which is what every reader of
/// this socket did before it could be asked anything else.
async fn answer(
    stream: tokio::net::UnixStream,
    published: Published,
    prices: Prices,
    wallet: Wallet,
    money: Option<crate::channel::Money>,
) -> Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut line = String::new();

    // Bounded, because "says nothing" has two shapes. A caller that closes its
    // writing half ends the read immediately; one that simply connects and
    // waits to be told something — which is what `nc` does, and what every
    // reader of this socket did before it could be asked anything — would
    // otherwise leave both sides waiting on each other until a timeout kills
    // the connection and the reader gets nothing at all.
    let _ = tokio::time::timeout(REQUEST_WAIT, BufReader::new(rx).read_line(&mut line)).await;

    let response = match line.trim() {
        "" => Response::Ok {
            data: serde_json::to_value(
                current(&published, &prices, &wallet, money.as_ref()).await,
            )?,
        },
        text => match serde_json::from_str::<Request>(text) {
            Ok(request) => run(request, &published, &prices, &wallet, money.as_ref()).await,
            Err(e) => Response::Error {
                message: format!("not a request this node understands: {e}"),
            },
        },
    };

    // A caller that said nothing gets the snapshot bare, exactly as before.
    // One that asked gets the answer wrapped, so that a failure is a failure
    // rather than a suspiciously empty snapshot.
    let mut body = match (&response, line.trim().is_empty()) {
        (Response::Ok { data }, true) => serde_json::to_vec(data)?,
        _ => serde_json::to_vec(&response)?,
    };
    body.push(b'\n');
    tx.write_all(&body).await?;
    tx.shutdown().await?;
    Ok(())
}

async fn run(
    request: Request,
    published: &Published,
    prices: &Prices,
    wallet: &Wallet,
    money: Option<&crate::channel::Money>,
) -> Response {
    match request {
        Request::ShowWallet => match serde_json::to_value(wallet.balances().await) {
            Ok(data) => Response::Ok {
                data: serde_json::json!({ "holdings": data }),
            },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::TopUp { amount, mint, unit } => {
            // Defaulting to the node's own money mint is the whole point: an
            // operator topping up is topping up *their* wallet, and should not
            // have to know a URL to do it.
            let Some((mint, unit)) = mint
                .map(|mint| (mint, unit.clone()))
                .or_else(|| money.map(|m| (m.mint.clone(), m.unit.clone())))
            else {
                return Response::Error {
                    message: "this node has no money mint configured; name one".into(),
                };
            };
            top_up(wallet.clone(), mint, unit, amount).await
        }
        Request::Claim { mint, unit } => {
            let Some((mint, unit)) = mint
                .map(|mint| (mint, unit.clone()))
                .or_else(|| money.map(|m| (m.mint.clone(), m.unit.clone())))
            else {
                return Response::Error {
                    message: "this node has no money mint configured; name one".into(),
                };
            };
            match wallet.claim_pending(&mint, &unit).await {
                Ok(claimed) => Response::Ok {
                    data: serde_json::json!({ "claimed": claimed, "unit": unit, "mint": mint }),
                },
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            }
        }
        Request::Snapshot => {
            match serde_json::to_value(current(published, prices, wallet, money).await) {
                Ok(data) => Response::Ok { data },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::ShowPrices => match serde_json::to_value(prices.listed()) {
            Ok(data) => Response::Ok {
                data: serde_json::json!({ "accepts": data }),
            },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::SetPrice {
            mint,
            unit,
            bytes_per_unit,
        } => {
            prices.set(&mint, &unit, bytes_per_unit);
            // Worth a line in the log: it changes what the node charges, and
            // the next operator to read the logs will want to know when.
            tracing::info!(%mint, %unit, bytes_per_unit, "a market price was changed");
            match serde_json::to_value(prices.listed()) {
                Ok(data) => Response::Ok {
                    data: serde_json::json!({ "accepts": data }),
                },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
    }
}

/// The published snapshot, with what the market and the wallet hold right now.
async fn current(
    published: &Published,
    prices: &Prices,
    wallet: &Wallet,
    money: Option<&crate::channel::Money>,
) -> Snapshot {
    let mut snapshot = (**published.load()).clone();
    snapshot.accepts = prices.listed();
    snapshot.holdings = wallet.balances().await;
    snapshot.money_mint = money.map(|m| Accepted {
        mint: m.mint.clone(),
        unit: m.unit.clone(),
        // Not a price: this entry says where the node buys money, not what it
        // sells capacity for. Zero rather than a number that would read as one.
        bytes_per_unit: 0,
    });
    snapshot
}

/// Buy money, and collect it in the background once the invoice is paid.
///
/// The invoice comes back immediately because somebody has to pay it, and that
/// somebody is a person with a Lightning wallet. Claiming what it bought is the
/// node's job, so the caller has nothing to confirm: pay, and watch the
/// balance.
async fn top_up(wallet: Wallet, mint: String, unit: String, amount: u64) -> Response {
    let top_up = match wallet.top_up(&mint, &unit, amount).await {
        Ok(top_up) => top_up,
        Err(e) => {
            return Response::Error {
                message: format!("{e:#}"),
            };
        }
    };

    tokio::spawn({
        let wallet = wallet.clone();
        let top_up: TopUp = top_up.clone();
        async move {
            // Backing off rather than asking once a second. A public mint is
            // somebody else's server and several of them rate-limit hard: at
            // one poll a second, 21mint started answering
            // `{"detail":"Rate limit exceeded."}` to every status request,
            // which the client cannot parse as a quote — so a top-up that had
            // been *paid* was never collected, and the polling was the reason.
            //
            // A person paying an invoice takes tens of seconds anyway, so the
            // early polls are the useless ones.
            let mut wait = std::time::Duration::from_secs(2);
            let deadline = std::time::Instant::now() + TOP_UP_WINDOW;
            let mut last: Option<String> = None;

            while std::time::Instant::now() < deadline {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(30));

                match wallet.collect(&top_up).await {
                    Ok(minted) => {
                        tracing::info!(minted, unit = %top_up.unit, "top-up collected");
                        return;
                    }
                    // Kept and reported once at the end rather than every time
                    // round: "not paid yet" is the expected answer for most of
                    // this loop, and a log line per poll would bury the one
                    // that is not.
                    Err(e) => last = Some(format!("{e:#}")),
                }
            }

            tracing::warn!(
                amount = top_up.amount,
                unit = %top_up.unit,
                quote = %top_up.quote,
                last_error = last.unwrap_or_default(),
                "gave up waiting for a top-up; if it was paid, claim it"
            );
        }
    });

    match serde_json::to_value(&top_up) {
        Ok(data) => Response::Ok { data },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

/// How long to keep watching for a top-up: a person, a phone, an invoice.
///
/// Giving up is not losing the money — the quote stays in the wallet database
/// and [`Request::Claim`] collects it whenever the mint will talk again.
const TOP_UP_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// Read one snapshot from a node's control socket.
pub async fn fetch(path: &Path) -> Result<Snapshot> {
    let body = exchange(path, None).await?;
    serde_json::from_str(&body).context("parse the snapshot")
}

/// Send one request to a node's control socket and return what it answered.
pub async fn send(path: &Path, request: &Request) -> Result<Response> {
    let body = exchange(path, Some(serde_json::to_string(request)?)).await?;
    serde_json::from_str(&body).with_context(|| format!("parse the answer: {body}"))
}

/// One connection, one optional request, one answer.
///
/// The writing half is closed either way, which is what tells a node that says
/// nothing that nothing is coming: the server reads a line, and a half-closed
/// socket ends that read rather than leaving both sides waiting on each other.
async fn exchange(path: &Path, request: Option<String>) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    let (mut rx, mut tx) = stream.into_split();

    if let Some(request) = request {
        tx.write_all(request.as_bytes())
            .await
            .context("write the request")?;
        tx.write_all(b"\n").await.context("write the request")?;
    }
    tx.shutdown().await.context("finish writing")?;

    let mut body = String::new();
    rx.read_to_string(&mut body)
        .await
        .context("read the answer")?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: &str = "https://mint.example/Bitcoin";

    fn priced(bytes_per_unit: u64) -> Prices {
        Prices::new([Accepted {
            mint: MINT.into(),
            unit: "sat".into(),
            bytes_per_unit,
        }])
    }

    /// Serve a socket in a temporary directory, and hand back the path.
    async fn serving(prices: Prices) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let seed: u64 = prices
            .listed()
            .first()
            .map(|a| a.bytes_per_unit)
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "tollgate-control-test-{}.sock",
            std::process::id() as u64 + seed
        ));
        let published: Published = Arc::new(ArcSwap::from_pointee(Snapshot {
            pubkey: "02aa".into(),
            unit: "byte".into(),
            ..Snapshot::default()
        }));

        // A wallet of its own per test, in the same temporary place, so nothing
        // here touches a real balance.
        let wallet = Wallet::open(path.with_extension("wallet"), [3u8; 64], "byte")
            .await
            .expect("open a wallet");

        let serve_path = path.clone();
        let task = tokio::spawn(async move {
            let _ = serve(
                &serve_path,
                published,
                prices,
                wallet,
                None,
                std::future::pending::<()>(),
            )
            .await;
        });

        // Bound rather than fixed: binding a Unix socket is fast, but a loaded
        // machine is a loaded machine.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (path, task)
    }

    #[test]
    fn a_service_managed_socket_is_looked_for_first() {
        // /run is where a service manager puts it and is not writable by an
        // ordinary user, which is what makes finding one there meaningful.
        let candidates = socket_candidates();
        assert_eq!(
            candidates.first().unwrap(),
            &PathBuf::from("/run/tollgate.sock")
        );
        assert_eq!(
            candidates.last().unwrap(),
            &PathBuf::from("/tmp/tollgated.sock"),
            "the fallback that exists everywhere goes last"
        );
    }

    #[test]
    fn a_socket_that_is_not_anywhere_says_where_it_looked() {
        // The old failure was "/tmp/tollgated.sock: No such file or directory",
        // which told an operator nothing about the two other places a node
        // might have put it.
        let Err(e) = find_socket() else {
            // A node really is running on this machine; nothing to assert.
            return;
        };
        let message = format!("{e}");
        assert!(message.contains("/run/tollgate.sock"), "{message}");
        assert!(message.contains("--socket"), "{message}");
    }

    #[tokio::test]
    async fn a_caller_that_says_nothing_still_gets_the_snapshot() {
        // Every reader of this socket did exactly this before it could be asked
        // anything, and they must not have to change.
        let (path, task) = serving(priced(1_000_000)).await;

        let snapshot = fetch(&path).await.expect("fetch");
        assert_eq!(snapshot.pubkey, "02aa");
        assert_eq!(
            snapshot.accepts.first().map(|a| a.bytes_per_unit),
            Some(1_000_000),
            "the price list is served alongside what the node is doing"
        );

        task.abort();
    }

    #[tokio::test]
    async fn the_price_can_be_changed_through_the_socket() {
        let prices = priced(2_000_000);
        let (path, task) = serving(prices.clone()).await;

        let answer = send(
            &path,
            &Request::SetPrice {
                mint: MINT.into(),
                unit: "sat".into(),
                bytes_per_unit: 500_000,
            },
        )
        .await
        .expect("set the price");
        assert!(matches!(answer, Response::Ok { .. }));

        // The market holds the same list, not a copy of it: what the socket
        // changed is what the next swap is priced against.
        assert_eq!(prices.bytes_per_unit(MINT, "sat"), Some(500_000));
        assert_eq!(
            fetch(&path)
                .await
                .expect("fetch")
                .accepts
                .first()
                .map(|a| a.bytes_per_unit),
            Some(500_000)
        );

        task.abort();
    }

    #[tokio::test]
    async fn an_issuer_can_be_refused_through_the_socket() {
        // Zero is how an operator stops taking a mint's paper — the entry goes
        // away rather than becoming a very expensive one.
        let prices = priced(4_000_000);
        let (path, task) = serving(prices.clone()).await;

        send(
            &path,
            &Request::SetPrice {
                mint: MINT.into(),
                unit: "sat".into(),
                bytes_per_unit: 0,
            },
        )
        .await
        .expect("refuse the issuer");

        assert!(!prices.is_selling());
        assert!(fetch(&path).await.expect("fetch").accepts.is_empty());

        task.abort();
    }

    #[tokio::test]
    async fn a_request_that_makes_no_sense_is_refused_rather_than_guessed_at() {
        let (path, task) = serving(priced(3_000_000)).await;

        let body = exchange(&path, Some("{\"command\":\"drop_everything\"}".into()))
            .await
            .expect("exchange");
        let answer: Response = serde_json::from_str(&body).expect("an answer");
        assert!(
            matches!(answer, Response::Error { .. }),
            "an unknown command must not read as success: {body}"
        );

        task.abort();
    }
}
