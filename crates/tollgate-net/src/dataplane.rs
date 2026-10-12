//! The data plane: the resource itself.
//!
//! A separate socket from the control plane, carrying the bytes that are being
//! bought and sold. Nothing here speaks TollGate — it is the traffic, not the
//! protocol, and it exists so the demo shapes and meters **real** bytes rather
//! than a number in a spreadsheet.
//!
//! Each side writes to the other at whatever its shaper currently permits, and
//! counts what it writes as delivered and what it reads as received. Those are
//! the same two counters core draws grants down against, so the loop closes:
//! demand drives a purchase, the purchase raises the shaping rate, the shaper
//! releases more bytes, and the throughput the operator sees follows.
//!
//! In a real deployment this is replaced by nftables and a TUN device, or by a
//! FIPS delivery filter. The layers above it do not change.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tollgate_protocol::PubKey;
use tracing::debug;

use crate::adapter::Loopback;

/// How often the writer wakes to release what the bucket has accrued.
///
/// Short enough that a rate change shows up promptly, long enough that a
/// gigabit-scale rate is not chopped into thousands of tiny writes.
const WRITE_INTERVAL: Duration = Duration::from_millis(50);

/// Largest single write. Filler is meaningless, so this only bounds how much
/// memory a burst touches.
const CHUNK: usize = 64 * 1024;

/// Accept data-plane connections forever.
pub async fn listen(listener: TcpListener, adapter: Arc<Loopback>) -> Result<()> {
    loop {
        let (stream, addr) = listener.accept().await.context("accept")?;
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move {
            if let Err(e) = accept_one(stream, adapter).await {
                debug!(%addr, error = %e, "data connection ended");
            }
        });
    }
}

async fn accept_one(mut stream: TcpStream, adapter: Arc<Loopback>) -> Result<()> {
    // The caller identifies itself with its raw compressed key. This is not
    // authentication — the control plane and the layer below it deal with that.
    // It only says which meter the bytes belong to.
    let mut key = [0u8; 33];
    stream
        .read_exact(&mut key)
        .await
        .context("peer did not identify itself")?;
    run(stream, PubKey(key), adapter).await
}

/// Open a data-plane connection to a peer.
pub async fn dial(addr: &str, local: PubKey, peer: PubKey, adapter: Arc<Loopback>) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("dial data plane at {addr}"))?;
    stream
        .write_all(&local.0)
        .await
        .context("identify to peer")?;
    run(stream, peer, adapter).await
}

/// Stream in both directions until either side stops.
async fn run(stream: TcpStream, peer: PubKey, adapter: Arc<Loopback>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut rx, mut tx) = stream.into_split();

    let writer_adapter = Arc::clone(&adapter);
    let writer = tokio::spawn(async move {
        let filler = vec![0u8; CHUNK];
        let mut ticker = tokio::time::interval(WRITE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            // Everything the shaper will release this interval. `take_allowance`
            // handles the accounting; here we only move the bytes.
            let mut owed = writer_adapter.take_allowance(peer, WRITE_INTERVAL.as_millis() as u64);
            while owed > 0 {
                let n = owed.min(CHUNK as u64) as usize;
                if tx.write_all(&filler[..n]).await.is_err() {
                    return;
                }
                writer_adapter.record_delivered(peer, n as u64);
                owed -= n as u64;
            }
        }
    });

    let mut buf = vec![0u8; CHUNK];
    let result = loop {
        match rx.read(&mut buf).await {
            Ok(0) => break Ok(()),
            Ok(n) => adapter.record_received(peer, n as u64),
            Err(e) => break Err(e),
        }
    };

    writer.abort();
    match result {
        Ok(()) => Ok(()),
        Err(e) => bail!(e),
    }
}

/// Keep a data-plane connection to a peer up for as long as it will have us.
///
/// A peering is a standing relationship, so a dropped connection is a reason to
/// wait and redial rather than to give up.
pub fn keep_dialing(control_endpoint: String, local: PubKey, peer: PubKey, adapter: Arc<Loopback>) {
    tokio::spawn(async move {
        let Some(addr) = one_port_up(&control_endpoint) else {
            tracing::warn!(%control_endpoint, "cannot derive a data-plane address");
            return;
        };
        loop {
            if let Err(e) = dial(&addr, local, peer, Arc::clone(&adapter)).await {
                debug!(%addr, error = %e, "data dial failed");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

/// The data plane sits one port above the control plane.
pub fn one_port_up(control: &str) -> Option<String> {
    let (host, port) = control.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    Some(format!("{host}:{}", port.checked_add(1)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_plane_is_one_port_above_the_control_plane() {
        assert_eq!(
            one_port_up("127.0.0.1:4747").as_deref(),
            Some("127.0.0.1:4748")
        );
    }

    #[test]
    fn an_endpoint_without_a_port_has_no_data_plane() {
        assert_eq!(one_port_up("127.0.0.1"), None);
    }
}
