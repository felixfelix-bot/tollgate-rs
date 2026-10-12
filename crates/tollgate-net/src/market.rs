//! Selling this node's own vouchers, and buying a peer's.
//!
//! **Not part of the TollGate protocol.** No TollGate message buys, sells or
//! swaps a voucher; acquiring them happens before a session and outside it. A
//! node that offers no market at all still works — its peers simply have to
//! arrive holding its vouchers already.
//!
//! # One trade, in one direction of trust
//!
//! A buyer sends money — somebody else's sat-denominated Cashu token — and
//! gets this node's byte vouchers back. That is the whole mechanism. There is
//! no quote to expire, no invoice, no Lightning node and no atomicity
//! construction, because the buyer is already trusting this node with
//! something much larger: that it will carry the traffic it just sold. A
//! router that would pocket a swap would simply stop forwarding instead.
//!
//! A peer that holds only sats in a Lightning wallet is not stuck: it mints
//! sat-denominated ecash at any mint this node accepts, and swaps that. Paying
//! over Lightning is somebody else's mint's problem, which is exactly where it
//! belongs.
//!
//! # Priced per issuer, not per unit
//!
//! What this node takes, and what it gives for it, is a list: one entry per
//! mint, with a price in bytes per unit of that mint's paper. That is the
//! point rather than an accident of the schema. A sat from a mint you expect
//! to honour its tokens is worth more than a sat from one you do not, and
//! `docs/design/market/voucher-price-signal.md` is about exactly that
//! difference. An operator prices issuer risk by pricing issuers.
//!
//! The unit is stated per entry and defaults to `sat`, because a mint may run
//! several keysets: a token carries its own unit, but "a million bytes per
//! unit" means different things against a sat and a cent, so the offer has to
//! say which it is. Paper in a unit this node has not priced is refused rather
//! than guessed at.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use cdk::mint::Mint;
use cdk::nuts::{BlindSignature, BlindedMessage};
use cdk_common::nuts::{KeySetInfo, Token};

use crate::wallet::Wallet;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// What this node takes as payment, and at what price.
pub const INFO_PATH: &str = "/tollgate/market/v1/info";
/// Money in, vouchers out.
pub const SWAP_PATH: &str = "/tollgate/market/v1/swap";

/// One issuer's paper, and what a unit of it buys here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    /// The mint whose tokens this node will take.
    pub mint: String,
    /// The unit of that mint's keyset. A token in any other unit is refused.
    pub unit: String,
    /// Units of capacity one unit of that paper buys. Zero refuses it.
    pub bytes_per_unit: u64,
}

/// Everything this node will take as payment.
///
/// Shared and mutable because it is a live setting: the configuration file
/// fills it at startup and the control socket changes it while the node runs.
/// An operator whose uplink has become scarce, or who has stopped trusting an
/// issuer, should not have to restart the thing selling capacity.
#[derive(Debug, Clone, Default)]
pub struct Prices(Arc<RwLock<BTreeMap<(String, String), u64>>>);

impl Prices {
    /// Build a price list.
    pub fn new(accepted: impl IntoIterator<Item = Accepted>) -> Self {
        let prices = Self::default();
        for entry in accepted {
            prices.set(&entry.mint, &entry.unit, entry.bytes_per_unit);
        }
        prices
    }

    /// What a unit of one issuer's paper buys, if this node takes it at all.
    pub fn bytes_per_unit(&self, mint: &str, unit: &str) -> Option<u64> {
        self.0
            .read()
            .expect("not poisoned")
            .get(&(normalise(mint), unit.to_string()))
            .copied()
            .filter(|price| *price > 0)
    }

    /// Set what a unit of one issuer's paper buys. Zero stops taking it.
    pub fn set(&self, mint: &str, unit: &str, bytes_per_unit: u64) {
        let mut prices = self.0.write().expect("not poisoned");
        let key = (normalise(mint), unit.to_string());
        if bytes_per_unit == 0 {
            prices.remove(&key);
        } else {
            prices.insert(key, bytes_per_unit);
        }
    }

    /// The whole list, for the info endpoint and for anything displaying it.
    pub fn listed(&self) -> Vec<Accepted> {
        self.0
            .read()
            .expect("not poisoned")
            .iter()
            .map(|((mint, unit), bytes_per_unit)| Accepted {
                mint: mint.clone(),
                unit: unit.clone(),
                bytes_per_unit: *bytes_per_unit,
            })
            .collect()
    }

    /// Whether this node is selling at all.
    pub fn is_selling(&self) -> bool {
        !self.0.read().expect("not poisoned").is_empty()
    }

    /// What `bytes` cost in one issuer's paper, rounded up.
    ///
    /// Never free: a buyer asking for a byte at a megabyte per unit owes one
    /// unit rather than nothing. Rounding down would let a buyer take capacity
    /// a byte at a time and pay for none of it.
    pub fn units_for(&self, mint: &str, unit: &str, bytes: u64) -> Option<u64> {
        let per_unit = self.bytes_per_unit(mint, unit)?;
        if bytes == 0 {
            return None;
        }
        Some((bytes as u128).div_ceil(per_unit as u128).max(1) as u64)
    }
}

/// Mint URLs differ by a trailing slash more often than by anything else.
fn normalise(mint_url: &str) -> String {
    mint_url.trim_end_matches('/').to_string()
}

/// What this market will do.
#[derive(Debug, Serialize, Deserialize)]
pub struct Info {
    /// The unit this node's own vouchers denominate in.
    pub unit: String,
    /// This node's mint, whose paper is what a buyer gets.
    pub mint: String,
    /// What it takes as payment, and at what price.
    pub accepts: Vec<Accepted>,
}

/// Money in, blinded outputs to sign.
#[derive(Debug, Deserialize, Serialize)]
pub struct SwapRequest {
    /// A Cashu token from a mint this node accepts.
    pub token: String,
    /// Blinded outputs, summing to what the token buys at the price in force.
    pub outputs: Vec<BlindedMessage>,
}

/// The signatures over them.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwapResponse {
    /// One per output, in the same order.
    pub signatures: Vec<BlindSignature>,
}

/// The market's state.
#[derive(Clone)]
struct Market {
    mint: Arc<Mint>,
    unit: String,
    mint_url: String,
    prices: Prices,
    /// Where the money taken goes. Custody is not the market's job — it
    /// prices, takes payment, and hands it straight over.
    wallet: Wallet,
    /// Ceiling on a single swap, from the mint's own limit.
    max_amount: u64,
}

/// The market router, mounted alongside the mint.
pub fn router(
    mint: Arc<Mint>,
    unit: String,
    mint_url: String,
    prices: Prices,
    wallet: Wallet,
    max_amount: u64,
) -> Router {
    Router::new()
        .route(INFO_PATH, get(info))
        .route(SWAP_PATH, post(swap))
        .with_state(Market {
            mint,
            unit,
            mint_url,
            prices,
            wallet,
            max_amount,
        })
}

type Failure = (axum::http::StatusCode, String);

fn bad_request(message: impl Into<String>) -> Failure {
    (axum::http::StatusCode::BAD_REQUEST, message.into())
}

async fn info(State(market): State<Market>) -> Json<Info> {
    Json(Info {
        unit: market.unit.clone(),
        mint: market.mint_url.clone(),
        accepts: market.prices.listed(),
    })
}

/// Take payment and sign what it bought.
///
/// The money is redeemed before anything is signed, and redeeming it at the
/// issuing mint is what makes it payment rather than a claim: proofs that were
/// already spent, or were never signed, do not survive the swap.
async fn swap(
    State(market): State<Market>,
    Json(request): Json<SwapRequest>,
) -> Result<Json<SwapResponse>, Failure> {
    let token = Token::from_str(request.token.trim())
        .map_err(|e| bad_request(format!("that is not a Cashu token: {e}")))?;

    let paid_mint = normalise(
        &token
            .mint_url()
            .map_err(|e| bad_request(e.to_string()))?
            .to_string(),
    );
    let paid_unit = token
        .unit()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "sat".into());
    let paid: u64 = token
        .value()
        .map_err(|e| bad_request(e.to_string()))?
        .into();

    let Some(bytes_per_unit) = market.prices.bytes_per_unit(&paid_mint, &paid_unit) else {
        return Err(bad_request(format!(
            "this node does not take {paid_unit} from {paid_mint}"
        )));
    };

    let bought = paid
        .checked_mul(bytes_per_unit)
        .filter(|b| *b <= market.max_amount)
        .ok_or_else(|| {
            bad_request(format!(
                "{paid} {paid_unit} is more capacity than this node will sell at once"
            ))
        })?;

    let wanted: u64 = request
        .outputs
        .iter()
        .map(|o| u64::from(o.amount))
        .fold(0, u64::saturating_add);
    if wanted != bought {
        return Err(bad_request(format!(
            "{paid} {paid_unit} buys {bought} {} here, and the outputs come to {wanted}",
            market.unit
        )));
    }

    // Take the money first, and take it properly: depositing swaps the proofs
    // at the issuing mint, which both proves they were good and stops the payer
    // spending them again while this request is in flight. It is also what
    // makes this payment rather than a receipt — a balance is what a node has
    // actually been paid.
    let held = market
        .wallet
        .deposit(&request.token)
        .await
        .map_err(|e| bad_request(format!("that payment did not clear: {e:#}")))?;

    match market.mint.blind_sign(request.outputs).await {
        Ok(signatures) => {
            info!(
                paid,
                unit = %paid_unit,
                from = %paid_mint,
                sold = bought,
                "sold capacity"
            );
            debug!(banked = held.amount, unit = %held.unit, "payment banked");
            Ok(Json(SwapResponse { signatures }))
        }
        Err(e) => {
            // The money is already ours and the buyer has nothing. Loud,
            // because it is the one outcome this design cannot make good
            // automatically — and the reason a buyer is trusting the seller.
            warn!(
                paid,
                unit = %paid_unit,
                error = %e,
                "took payment and could not issue against it"
            );
            Err(bad_request(e.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// The buying side
// ---------------------------------------------------------------------------

/// Buy `amount` of a peer's vouchers, paying with `money`.
///
/// `money` is a token from a mint the peer accepts — this node's own holdings,
/// obtained wherever money is obtained. What comes back is a token of the
/// peer's paper, which is what funds a channel to pay it.
pub fn buy(
    market_url: &str,
    amount: u64,
    unit: &str,
    keyset_info: &str,
    money: &str,
) -> Result<String> {
    let blinded = cdk_spilman::create_plain_blinded_messages(amount, keyset_info)
        .map_err(|e| anyhow!("blind {amount} {unit}: {e}"))?;
    let blinded: serde_json::Value = serde_json::from_str(&blinded).context("blinded messages")?;
    let secrets = blinded["secrets_with_blinding"].to_string();

    let body = http(
        "POST",
        &format!("{market_url}{SWAP_PATH}"),
        &serde_json::json!({
            "token": money,
            "outputs": blinded["blinded_messages"],
        })
        .to_string(),
    )
    .map_err(|e| anyhow!("buy {amount} {unit} from {market_url}: {e}"))?;

    let response: serde_json::Value = serde_json::from_str(&body).context("swap response")?;
    let signatures = response["signatures"]
        .as_array()
        .ok_or_else(|| anyhow!("{market_url} sold nothing for that payment: {body}"))?;

    let proofs =
        cdk_spilman::construct_proofs(&serde_json::to_string(signatures)?, &secrets, keyset_info)
            .map_err(|e| anyhow!("unblind {amount} {unit}: {e}"))?;

    cdk_spilman::build_cashu_b_token(market_url, unit, &proofs)
        .map_err(|e| anyhow!("build a token: {e}"))
}

/// What a market takes as payment.
pub fn info_of(market_url: &str) -> Result<Info> {
    let body = http("GET", &format!("{market_url}{INFO_PATH}"), "")
        .map_err(|e| anyhow!("ask {market_url} what it takes: {e}"))?;
    serde_json::from_str(&body).with_context(|| format!("market info from {market_url}: {body}"))
}

/// Wait until a mint says one of its quotes has been paid.
///
/// Bounded, because nothing downstream can start until it lands and a caller
/// retrying the whole purchase is better than one blocked forever. Where the
/// money mint settles its own invoices this returns at once; where a human has
/// to pay one, it gives up and the operator pays it from `tolltop`.
pub fn wait_until_paid(mint_url: &str, quote: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + PAYMENT_TIMEOUT;
    let url = format!("{mint_url}/v1/mint/quote/bolt11/{quote}");

    while std::time::Instant::now() < deadline {
        let body = http("GET", &url, "").map_err(|e| anyhow!("check a mint quote: {e}"))?;
        let status: serde_json::Value =
            serde_json::from_str(&body).with_context(|| format!("quote status: {body}"))?;

        let paid = matches!(status["state"].as_str(), Some("PAID") | Some("ISSUED"))
            || status["paid"].as_bool() == Some(true)
            || status["amount_paid"].as_u64().is_some_and(|a| a > 0);
        if paid {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    bail!("{mint_url} has not seen payment for quote {quote}")
}

/// How long to wait for an invoice to be paid.
const PAYMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Every keyset a mint has, active or not.
///
/// Both things that read a token need this: the active one to sign against,
/// and the whole list to resolve the short keyset ids inside a token.
fn keysets(mint_url: &str) -> Result<Vec<KeySetInfo>> {
    let body = http("GET", &format!("{mint_url}/v1/keysets"), "")
        .map_err(|e| anyhow!("list keysets at {mint_url}: {e}"))?;
    let listing: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("keyset listing from {mint_url}"))?;
    serde_json::from_value(listing["keysets"].clone())
        .with_context(|| format!("keyset listing from {mint_url}: {listing}"))
}

fn active_keyset_from(keysets: &[KeySetInfo], unit: &str) -> Option<String> {
    keysets
        .iter()
        .find(|k| k.active && k.unit.to_string() == unit)
        .map(|k| k.id.to_string())
}

/// The id of a mint's active keyset for `unit`.
///
/// A purchase is signed by one keyset, and it has to be the active one: a mint
/// keeps older keysets readable so outstanding vouchers stay redeemable, but it
/// will not sign new ones against them.
pub fn active_keyset(mint_url: &str, unit: &str) -> Result<String> {
    active_keyset_from(&keysets(mint_url)?, unit)
        .ok_or_else(|| anyhow!("{mint_url} has no active {unit} keyset"))
}

/// A blocking HTTP call, for the paths the synchronous channel backend drives.
///
/// The node runs every backend call on a blocking thread, so a blocking client
/// here does not stall the runtime.
fn http(method: &str, url: &str, body: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    let request = match method {
        "GET" => client.get(url),
        _ => client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_owned()),
    };
    let response: reqwest::blocking::Response = request.send().map_err(|e| e.to_string())?;
    response.text().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced() -> Prices {
        Prices::new([
            Accepted {
                mint: "https://mint.minibits.cash/Bitcoin".into(),
                unit: "sat".into(),
                bytes_per_unit: 1_000_000,
            },
            Accepted {
                mint: "https://nofees.testnut.cashu.space".into(),
                unit: "sat".into(),
                bytes_per_unit: 700_000,
            },
        ])
    }

    #[test]
    fn paper_this_node_does_not_take_has_no_price() {
        let prices = priced();
        assert_eq!(
            prices.bytes_per_unit("https://someone.else/mint", "sat"),
            None
        );
        // A mint it does take, in a unit it does not.
        assert_eq!(
            prices.bytes_per_unit("https://mint.minibits.cash/Bitcoin", "usd"),
            None,
            "a unit that was never priced is not the same as a cheap one"
        );
    }

    #[test]
    fn each_issuer_is_priced_on_its_own() {
        // The whole point of a list: paper you trust less buys less.
        let prices = priced();
        assert_eq!(
            prices.bytes_per_unit("https://mint.minibits.cash/Bitcoin", "sat"),
            Some(1_000_000)
        );
        assert_eq!(
            prices.bytes_per_unit("https://nofees.testnut.cashu.space", "sat"),
            Some(700_000)
        );
    }

    #[test]
    fn a_trailing_slash_is_the_same_mint() {
        let prices = priced();
        assert_eq!(
            prices.bytes_per_unit("https://mint.minibits.cash/Bitcoin/", "sat"),
            Some(1_000_000)
        );
    }

    #[test]
    fn a_price_of_zero_stops_taking_that_issuers_paper() {
        let prices = priced();
        prices.set("https://nofees.testnut.cashu.space", "sat", 0);
        assert_eq!(
            prices.bytes_per_unit("https://nofees.testnut.cashu.space", "sat"),
            None
        );
        assert!(prices.is_selling(), "the other issuer is still taken");
        assert_eq!(prices.listed().len(), 1);
    }

    #[test]
    fn what_a_quantity_costs_is_rounded_up() {
        let prices = priced();
        let mint = "https://mint.minibits.cash/Bitcoin";
        assert_eq!(prices.units_for(mint, "sat", 1_000_000), Some(1));
        assert_eq!(prices.units_for(mint, "sat", 1_000_001), Some(2));
        // Rounding down would let a buyer take capacity a byte at a time and
        // pay for none of it.
        assert_eq!(prices.units_for(mint, "sat", 1), Some(1));
        assert_eq!(prices.units_for(mint, "sat", 0), None);
        assert_eq!(prices.units_for("https://elsewhere", "sat", 1_000), None);
    }

    #[test]
    fn the_price_list_can_change_while_the_node_runs() {
        let prices = priced();
        let elsewhere = prices.clone();
        elsewhere.set("https://mint.minibits.cash/Bitcoin", "sat", 500_000);
        assert_eq!(
            prices.bytes_per_unit("https://mint.minibits.cash/Bitcoin", "sat"),
            Some(500_000)
        );
    }
}
