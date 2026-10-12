//! A byte source and sink, so a client can measure what it actually gets.
//!
//! **Not part of the TollGate protocol.** Nothing here is exchanged between
//! nodes, nothing is signed, and a node that serves none of it works exactly as
//! well. It is an operator's instrument, and the audience is a person with a
//! browser.
//!
//! # Why a node has to serve the bytes itself
//!
//! The obvious way to build a speed test is the way Fast.com does it: point the
//! client at a CDN and time the download. That measures the path from the
//! client to Netflix, which is nearly all the path this node does not control
//! and cannot sell. Worse, it measures it *through* the tollgate, so the number
//! moves for reasons — a congested upstream, a slow peering point — that have
//! nothing to do with what was bought.
//!
//! Serving the bytes from the node itself turns that around. The client pulls
//! from an address on the mesh, so the flow crosses exactly the hops that were
//! paid for and nothing else. Under `forwarding.mode: fips` that is also the
//! honest number in the other direction: FIPS is enforcing the transit policy
//! this node set for that peer, so what the page reads is the grant in force,
//! not an unshaped side channel around it.
//!
//! # What the measurement window is for
//!
//! A fresh flow over the mesh is slow twice over — TCP slow start, and a Noise
//! handshake before that — and a grant bought a moment ago may not be in force
//! for the first packets. Averaging from the first byte reports the ramp. So
//! the page discards a warm-up window and reports only what came after it, and
//! it opens several flows at once, because one TCP flow over an encrypted mesh
//! at a 1472-byte MTU will under-read a link that has more to give.
//!
//! # This is not free capacity
//!
//! The endpoints are unauthenticated: anything that can reach the listener can
//! pull bytes from it. On a node in `fips` mode that is the intended shape,
//! because the transit those bytes cross is already gated by what the peer
//! bought — the test consumes a client's own allowance rather than bypassing
//! it. On a node in any other mode it is a free firehose, which is why it is
//! off unless an operator turns it on, and why one request is capped.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// The page itself, served at the root of the speedtest listener.
pub const PAGE_PATH: &str = "/";
/// How this node wants to be measured.
pub const INFO_PATH: &str = "/tollgate/speedtest/v1/info";
/// Bytes out.
pub const DOWN_PATH: &str = "/tollgate/speedtest/v1/down";
/// Bytes in, discarded.
pub const UP_PATH: &str = "/tollgate/speedtest/v1/up";

/// How big a slice of filler is generated and then repeated.
///
/// Large enough that the per-chunk bookkeeping disappears against the copy, and
/// small enough to stay resident on a router with a few megabytes to spare.
const BLOCK_BYTES: usize = 64 * 1024;

/// How this node serves the test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Ceiling on a single request, in either direction.
    ///
    /// The client asks for a bounded slice and comes back for more, so this is
    /// a cap on what one request costs the node rather than on how long a test
    /// may run.
    pub max_bytes: u64,
    /// How many flows the page opens at once.
    pub streams: u8,
    /// How long to discard before counting.
    pub warmup_ms: u32,
    /// How long to count for, after the warm-up.
    pub duration_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            // Fast.com opens five, for the same reason: one flow does not fill
            // a link that has any distance in it.
            streams: 5,
            warmup_ms: 1_500,
            duration_ms: 10_000,
        }
    }
}

/// What the page reads before it starts, so an operator tunes one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// Ceiling on a single request.
    pub max_bytes: u64,
    /// How many flows to open.
    pub streams: u8,
    /// How long to discard before counting.
    pub warmup_ms: u32,
    /// How long to count for.
    pub duration_ms: u32,
}

/// What the sink says it swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Received {
    /// Bytes read off the request body.
    pub bytes: u64,
}

/// The test's state: the settings, and one block of filler to repeat.
#[derive(Clone)]
struct Speedtest {
    config: Arc<Config>,
    block: Bytes,
}

/// Query string of a download request.
#[derive(Debug, Deserialize)]
struct Wanted {
    /// How many bytes to send. Clamped to [`Config::max_bytes`].
    bytes: Option<u64>,
}

/// The speedtest router.
///
/// Mounted on a listener of its own rather than beside the mint, because the
/// two want different addresses: the mint answers peers that already know this
/// node, and this answers a browser on the mesh.
pub fn router(config: Config) -> Router {
    let state = Speedtest {
        config: Arc::new(config),
        block: filler(),
    };

    // The sink reads the body itself and stops at the cap, so axum's own limit
    // — 2 MiB by default — would refuse an upload run before the handler ever
    // saw it.
    let cap = state.config.max_bytes;

    Router::new()
        .route(PAGE_PATH, get(page))
        .route(INFO_PATH, get(info))
        .route(DOWN_PATH, get(down))
        .route(
            UP_PATH,
            post(up).layer(DefaultBodyLimit::max(cap.try_into().unwrap_or(usize::MAX))),
        )
        .with_state(state)
}

/// Serve the speedtest until `shutdown` resolves.
pub async fn serve(
    config: Config,
    listen: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind the speedtest on {listen}"))?;

    axum::serve(listener, router(config))
        .with_graceful_shutdown(shutdown)
        .await
        .context("serve the speedtest")
}

/// A block of filler to send over and over.
///
/// Hashed rather than zeroed, and repeated rather than generated per request.
/// Repetition is invisible to the measurement because nothing on this path
/// compresses — but a block of zeroes is not, and a link that ever did compress
/// would report a number no traffic could reproduce.
fn filler() -> Bytes {
    use sha2::{Digest, Sha256};

    let mut bytes = Vec::with_capacity(BLOCK_BYTES);
    let mut seed = Sha256::digest(b"tollgate-speedtest-filler");
    while bytes.len() < BLOCK_BYTES {
        bytes.extend_from_slice(&seed);
        seed = Sha256::digest(seed);
    }
    bytes.truncate(BLOCK_BYTES);
    Bytes::from(bytes)
}

async fn page() -> Html<&'static str> {
    Html(include_str!("../assets/speedtest.html"))
}

async fn info(State(test): State<Speedtest>) -> Json<Info> {
    Json(Info {
        max_bytes: test.config.max_bytes,
        streams: test.config.streams,
        warmup_ms: test.config.warmup_ms,
        duration_ms: test.config.duration_ms,
    })
}

/// Send a bounded run of filler.
///
/// The length is declared, so the client can tell a finished transfer from a
/// truncated one, and the body is streamed rather than assembled, so serving a
/// 256 MiB request does not cost 256 MiB of memory.
async fn down(State(test): State<Speedtest>, Query(wanted): Query<Wanted>) -> Response {
    let total = wanted
        .bytes
        .unwrap_or(test.config.max_bytes)
        .min(test.config.max_bytes);

    let stream = futures::stream::unfold((total, test.block), |(left, block)| async move {
        if left == 0 {
            return None;
        }
        let take = left.min(block.len() as u64) as usize;
        let chunk = block.slice(..take);
        Some((
            Ok::<Bytes, std::io::Error>(chunk),
            (left - take as u64, block),
        ))
    });

    (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_LENGTH, &total.to_string()),
            // Measuring a cache is measuring nothing. The client also varies the
            // query string, so this is the second of two defences rather than
            // the only one.
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Read a request body and throw it away.
///
/// Counted here rather than trusted from a header: a client that declares more
/// than it sends would otherwise be reporting a rate it never achieved.
async fn up(State(test): State<Speedtest>, request: Request) -> Response {
    let mut body = request.into_body().into_data_stream();
    let mut bytes = 0u64;

    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) => {
                bytes = bytes.saturating_add(chunk.len() as u64);
                if bytes > test.config.max_bytes {
                    debug!(bytes, cap = test.config.max_bytes, "upload over the cap");
                    return (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "this node takes at most {} bytes at once",
                            test.config.max_bytes
                        ),
                    )
                        .into_response();
                }
            }
            // A client that aborts a run mid-body is the normal ending, not an
            // error: the page stops its flows the moment the clock runs out.
            Err(e) => {
                debug!(error = %e, bytes, "upload ended early");
                break;
            }
        }
    }

    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(Received { bytes }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a router without a socket, the way axum's own tests do.
    async fn call(router: &Router, request: Request<Body>) -> (StatusCode, Bytes) {
        use tower::ServiceExt;
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the body");
        (status, body)
    }

    fn small() -> Config {
        Config {
            max_bytes: 4096,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn a_download_is_exactly_as_long_as_it_was_asked_for() {
        let router = router(small());
        let (status, body) = call(
            &router,
            Request::get(format!("{DOWN_PATH}?bytes=1000"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.len(), 1000);
    }

    #[tokio::test]
    async fn a_download_longer_than_a_block_keeps_going() {
        // The body is a repeated block, so the case that matters is the one
        // that has to repeat it.
        let router = router(small());
        let (_, body) = call(
            &router,
            Request::get(format!("{DOWN_PATH}?bytes=4096"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body.len(), 4096);
    }

    #[tokio::test]
    async fn one_request_cannot_cost_more_than_the_cap() {
        // Otherwise the listener is an unbounded byte firehose for anything
        // that can reach it.
        let router = router(small());
        let (_, body) = call(
            &router,
            Request::get(format!("{DOWN_PATH}?bytes=999999999"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body.len(), 4096);
    }

    #[tokio::test]
    async fn asking_for_nothing_in_particular_gets_the_cap() {
        let router = router(small());
        let (_, body) = call(
            &router,
            Request::get(DOWN_PATH).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(body.len(), 4096);
    }

    #[tokio::test]
    async fn filler_is_not_a_run_of_zeroes() {
        // A compressible payload would report a rate no real traffic reaches.
        let block = filler();
        assert_eq!(block.len(), BLOCK_BYTES);
        assert!(block.iter().any(|b| *b != 0));
    }

    #[tokio::test]
    async fn the_sink_counts_what_arrived() {
        let router = router(small());
        let (status, body) = call(
            &router,
            Request::post(UP_PATH)
                .body(Body::from(vec![7u8; 2048]))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received: Received = serde_json::from_slice(&body).expect("a byte count");
        assert_eq!(received.bytes, 2048);
    }

    #[tokio::test]
    async fn an_upload_over_the_cap_is_refused() {
        let router = router(small());
        let (status, _) = call(
            &router,
            Request::post(UP_PATH)
                .body(Body::from(vec![7u8; 8192]))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_page_tells_the_client_how_to_measure() {
        // The page ships defaults of its own, so this is what lets an operator
        // change them without editing the asset.
        let router = router(Config {
            streams: 3,
            warmup_ms: 250,
            duration_ms: 500,
            max_bytes: 4096,
        });
        let (status, body) = call(
            &router,
            Request::get(INFO_PATH).body(Body::empty()).unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let info: Info = serde_json::from_slice(&body).expect("settings");
        assert_eq!(info.streams, 3);
        assert_eq!(info.warmup_ms, 250);
        assert_eq!(info.duration_ms, 500);
        assert_eq!(info.max_bytes, 4096);
    }

    #[tokio::test]
    async fn the_page_needs_nothing_from_the_internet() {
        // The audience is a browser on a mesh that may have no route off it.
        let (status, body) = call(
            &router(small()),
            Request::get(PAGE_PATH).body(Body::empty()).unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let page = std::str::from_utf8(&body).expect("the page is text");
        assert!(!page.contains("http://"), "the page loads something remote");
        assert!(
            !page.contains("https://"),
            "the page loads something remote"
        );
    }
}
