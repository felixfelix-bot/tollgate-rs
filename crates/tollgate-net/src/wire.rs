//! The raw-TCP control plane: TollGate messages, length-prefixed.
//!
//! Peerings are adjacent by construction — hop-by-hop payment means the
//! counterparty is one link away — so there are no proxies or middleboxes in
//! between to justify dressing the protocol up as HTTP. Read two bytes, read
//! the body, hand it to the CBOR decoder.
//!
//! The transport carries no security burden. On FIPS a Noise IK handshake has
//! already authenticated and encrypted the link; on plain IP the operator wraps
//! the connection as it sees fit.
//!
//! What the transport can do is say whether a peer is who it claims to be — see
//! [`Identify`].

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tollgate_protocol::{FrameReader, MAX_FRAME_LEN, Message, PubKey, encode_frame};
use tracing::{debug, warn};

/// What the transport tells the node about.
#[derive(Debug)]
pub enum Wire {
    /// A link came up. The sender writes messages back to that peer.
    PeerUp {
        /// Who is on the other end.
        peer: PubKey,
        /// Where they reach us from.
        ///
        /// The session identifies a peer by public key; the kernel identifies
        /// it by address, and an adapter that gates forwarding needs both.
        addr: SocketAddr,
        /// Outbound queue for this link.
        tx: mpsc::Sender<Message>,
    },
    /// A message arrived.
    Message {
        /// Who sent it.
        peer: PubKey,
        /// What they sent.
        msg: Message,
    },
    /// The link went away, cleanly or otherwise.
    PeerDown {
        /// Who was on the other end.
        peer: PubKey,
    },
}

/// Whether the address a peer connects from has to agree with the key it
/// announces.
///
/// An Announce is unauthenticated: it is the first thing a stranger says. What
/// makes it costly to lie about is the address it was said from, and whether
/// that address means anything depends on the network underneath.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Identify {
    /// Take the peer at its word.
    ///
    /// On plain IP an address commits to nothing, so there is nothing to check
    /// against. A peer that claims a paying peer's key gets that peer's session
    /// — which is why an adapter that gates by address should not be run on an
    /// unwrapped network.
    #[default]
    Claimed,
    /// The connection has to come from the mesh address of the key announced.
    ///
    /// A FIPS address *is* a key: the mesh routes to it only for the node that
    /// completed a Noise IK handshake for it, so an impostor cannot receive at
    /// the address it would have to claim. Checking is local arithmetic — see
    /// [`crate::fips::address`] — and needs nothing from the FIPS daemon.
    Fips,
}

impl Identify {
    /// Refuse a peer that cannot be who it says it is.
    fn check(self, peer: PubKey, addr: SocketAddr) -> Result<()> {
        let Self::Fips = self else {
            return Ok(());
        };

        let expected = crate::fips::address(peer);
        let IpAddr::V6(actual) = addr.ip() else {
            bail!("a mesh peer has to reach us over fips0, and {addr} is not a mesh address");
        };
        if actual != expected {
            bail!("the key announced belongs at {expected}, not at {actual}");
        }
        Ok(())
    }
}

/// Accept control-plane connections forever.
///
/// A listener does not know who is calling until they say so, so it reads the
/// first frame — which the protocol requires to be an Announce — and takes the
/// peer's identity from it, subject to `identify`.
pub async fn listen(
    listener: TcpListener,
    node: mpsc::Sender<Wire>,
    identify: Identify,
) -> Result<()> {
    loop {
        let (stream, addr) = listener.accept().await.context("accept")?;
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = accept_one(stream, node, identify).await {
                debug!(%addr, error = %e, "control connection ended");
            }
        });
    }
}

async fn accept_one(
    mut stream: TcpStream,
    node: mpsc::Sender<Wire>,
    identify: Identify,
) -> Result<()> {
    let (peer, pending, reader) = read_announce(&mut stream).await?;
    run(stream, peer, pending, reader, node, identify).await
}

/// Read until the Announce turns up, and take the peer's identity from it.
///
/// Returns everything else that had already arrived, plus the reader itself.
/// Both matter: a peer sends Announce and Offer back to back, so they usually
/// land in one segment — and anything decoded here, or still sitting in the
/// reader's buffer, would be silently lost if identification started a fresh
/// reader of its own.
async fn read_announce(stream: &mut TcpStream) -> Result<(PubKey, Vec<Message>, FrameReader)> {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    let mut peer = None;
    let mut pending = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            bail!("peer closed before announcing itself");
        }
        reader.push(&buf[..n]);

        while let Some(msg) = reader.next_message() {
            let msg = msg.context("a message arriving before identification did not decode")?;
            match (&msg, peer) {
                // The Announce is still a message core has to see — it carries
                // the version and unit that decide whether to keep talking.
                (Message::Announce(a), None) => peer = Some(a.pubkey),
                (other, None) => bail!(
                    "expected Announce as the first message, got {:?}",
                    other.msg_type()
                ),
                _ => {}
            }
            // Anything behind the Announce is ordinary traffic; it just arrived
            // before we knew whose it was.
            pending.push(msg);
        }

        if let Some(peer) = peer {
            return Ok((peer, pending, reader));
        }
    }
}

/// Open a control-plane connection to a peer whose identity we already know
/// from configuration.
pub async fn dial(
    addr: &str,
    peer: PubKey,
    node: mpsc::Sender<Wire>,
    identify: Identify,
) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("dial {addr}"))?;
    run(stream, peer, Vec::new(), FrameReader::new(), node, identify).await
}

/// Pump one established connection until either side stops.
async fn run(
    stream: TcpStream,
    peer: PubKey,
    pending: Vec<Message>,
    reader: FrameReader,
    node: mpsc::Sender<Wire>,
    identify: Identify,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let addr = stream
        .peer_addr()
        .context("the peer's address is what an adapter gates on")?;

    // Before the session exists, because a session is what a stolen key would
    // be stealing. On a dialled connection the same check reads as configuration
    // sanity: an endpoint that is not the key's own address is the wrong node.
    if let Err(e) = identify.check(peer, addr) {
        warn!(%peer, %addr, error = %e, "refusing a peer whose address does not name its key");
        return Err(e);
    }

    let (mut rx_half, mut tx_half) = stream.into_split();

    // Depth enough to absorb a burst of setup messages without making core
    // wait, but bounded: a peer that will not read should not be able to make
    // us buffer without limit.
    let (tx, mut outbox) = mpsc::channel::<Message>(64);
    node.send(Wire::PeerUp { peer, addr, tx }).await.ok();

    // Anything read before the peer was identified still has to reach core,
    // in the order it arrived.
    for msg in pending {
        node.send(Wire::Message { peer, msg }).await.ok();
    }

    let writer = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(512);
        while let Some(msg) = outbox.recv().await {
            buf.clear();
            if let Err(e) = encode_frame(&msg, &mut buf) {
                warn!(error = %e, "dropping a message that would not encode");
                continue;
            }
            if tx_half.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = tx_half.shutdown().await;
    });

    let result = read_loop(&mut rx_half, reader, peer, &node).await;

    node.send(Wire::PeerDown { peer }).await.ok();
    writer.abort();
    result
}

/// Read frames until the peer stops.
///
/// Takes the reader by value rather than making its own: on an accepted
/// connection it already holds whatever arrived behind the Announce.
async fn read_loop(
    rx: &mut tokio::net::tcp::OwnedReadHalf,
    mut reader: FrameReader,
    peer: PubKey,
    node: &mpsc::Sender<Wire>,
) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];

    loop {
        let n = rx.read(&mut buf).await?;
        if n == 0 {
            // A bare FIN is an unclean disconnect: an orderly teardown sends
            // Disconnect first. Either way the cleanup is the same.
            return Ok(());
        }
        reader.push(&buf[..n]);

        while let Some(msg) = reader.next_message() {
            match msg {
                Ok(msg) => {
                    if node.send(Wire::Message { peer, msg }).await.is_err() {
                        return Ok(());
                    }
                }
                // One malformed frame does not poison the ones behind it: the
                // length prefix was intact, so the stream is still in sync.
                Err(e) => warn!(%peer, error = %e, "discarding a malformed message"),
            }
        }

        if reader.pending() > MAX_FRAME_LEN * 2 {
            bail!("peer is buffering more than two maximum frames without completing one");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> PubKey {
        let mut bytes = [0u8; 33];
        bytes[0] = 0x02;
        bytes[1..].fill(seed);
        PubKey(bytes)
    }

    fn on_mesh(peer: PubKey, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V6(crate::fips::address(peer)), port)
    }

    #[test]
    fn a_mesh_peer_at_its_own_address_is_accepted() {
        let peer = key(1);
        assert!(Identify::Fips.check(peer, on_mesh(peer, 4747)).is_ok());
    }

    #[test]
    fn a_mesh_peer_claiming_someone_elses_key_is_refused() {
        // The whole point: an address on the mesh is reachable only by the node
        // that handshook for it, so announcing a key that belongs elsewhere is
        // the one thing an impostor cannot make true.
        let victim = key(1);
        let impostor = key(2);
        assert!(
            Identify::Fips
                .check(victim, on_mesh(impostor, 4747))
                .is_err()
        );
    }

    #[test]
    fn a_mesh_peer_arriving_off_the_mesh_is_refused() {
        // A node whose control plane also answers on plain IP would otherwise
        // have a way in that skips the handshake entirely.
        let peer = key(1);
        let plain: SocketAddr = "10.0.0.7:4747".parse().unwrap();
        assert!(Identify::Fips.check(peer, plain).is_err());
    }

    #[test]
    fn without_a_mesh_underneath_any_address_will_do() {
        let peer = key(1);
        let plain: SocketAddr = "10.0.0.7:4747".parse().unwrap();
        assert!(Identify::Claimed.check(peer, plain).is_ok());
        assert!(Identify::Claimed.check(peer, on_mesh(key(2), 4747)).is_ok());
    }
}
