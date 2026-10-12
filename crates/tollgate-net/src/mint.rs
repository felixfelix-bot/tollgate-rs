//! This node's own mint.
//!
//! Every node issues vouchers against its own capacity and redeems them on
//! delivery. Redemption is a local spent-proof check — the node is the
//! authority on its own paper — which is why payment liveness and service
//! liveness fail together rather than separately.
//!
//! The keyset unit is the **byte**, not the sat. That is one of the three
//! things the voucher model changes, and it is what makes a proof a claim on
//! one unit of capacity rather than on money. A `1024` proof is a 1 KiB claim;
//! two of them make 2 KiB; they split and combine like any other Cashu token.
//!
//! # No Lightning here
//!
//! This mint has no payment processor at all, which is deliberate rather than
//! unfinished. Bolt11 is denominated in msat and this keyset is denominated in
//! bytes, so a mint quote would have to invent an exchange rate; and a peer
//! that wants to pay in money already has a way to hold money — somebody
//! else's sat-denominated mint. So the only way capacity is sold is through
//! [`crate::market`], which takes that paper and signs against it.
//!
//! Everything the mint itself does is real: the keysets, the blind signatures,
//! the DLEQ proofs and the spent-proof set.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use axum::Router;
use cdk::mint::{Mint, MintBuilder};
use cdk::nuts::CurrencyUnit;
use cdk_common::common::QuoteTTL;

/// How this node's mint is set up.
#[derive(Debug, Clone)]
pub struct MintConfig {
    /// URL peers reach it on, as advertised in our Offer.
    pub url: String,
    /// Quantity unit. `"byte"` for network forwarding.
    pub unit: String,
    /// Seed for the keyset, so a restart keeps issuing against the same keys.
    pub seed: Vec<u8>,
    /// Largest amount a single issuance may create.
    ///
    /// Not enforced by a payment processor — there is none — but kept as the
    /// ceiling the market applies to one swap.
    pub max_amount: u64,
}

/// The unit a keyset denominates in.
///
/// The resource fixes the unit, and every node selling the same resource
/// denominates the same way — that is what makes one issuer's vouchers
/// comparable to another's. `"sat"` is still spelled the Cashu way rather than
/// as a custom unit, so a sat-denominated node stays interoperable with
/// ordinary wallets.
pub fn currency_unit(unit: &str) -> CurrencyUnit {
    match unit {
        "sat" => CurrencyUnit::Sat,
        "msat" => CurrencyUnit::Msat,
        other => CurrencyUnit::Custom(other.into()),
    }
}

/// Build and start this node's mint.
pub async fn build(config: &MintConfig) -> Result<Mint> {
    let unit = currency_unit(&config.unit);
    let db = Arc::new(
        cdk_sqlite::mint::memory::empty()
            .await
            .context("open the mint database")?,
    );

    let mut builder = MintBuilder::new(db.clone())
        .with_name(format!("TollGate node at {}", config.url))
        .with_description("Vouchers: claims on this node's capacity".to_string())
        .with_urls(vec![config.url.clone()]);

    // The unit, and no way to buy it with money. A payment processor is how a
    // mint sells its own paper for something else, and there is nothing this
    // mint could quote: an invoice is written in msat and this keyset counts
    // bytes. Selling happens at the market, against paper from a mint that does
    // deal in money.
    builder
        .configure_unit(unit.clone(), Default::default())
        .map_err(|e| anyhow!("configure the {unit} keyset: {e}"))?;

    // No input fee. A fee would mean a voucher redeemed is worth slightly less
    // than a voucher issued, and delivery is one voucher per unit exactly.
    builder
        .set_unit_fee(&unit, 0)
        .map_err(|e| anyhow!("set the input fee: {e}"))?;

    let mint = builder
        .build_with_seed(db.clone(), &config.seed)
        .await
        .context("build the mint")?;

    mint.set_quote_ttl(QuoteTTL::new(10_000, 10_000))
        .await
        .context("set quote TTLs")?;

    if !mint.get_active_keysets().contains_key(&unit) {
        return Err(anyhow!(
            "the mint came up with no active keyset for {unit}; the unit is probably not supported"
        ));
    }

    mint.start().await.context("start the mint")?;
    Ok(mint)
}

/// Serve the mint over HTTP until `shutdown` resolves.
///
/// A peer funds its channel against **our** mint, so this has to be reachable
/// by peers — which it always is, because it is the node they are already
/// talking to.
pub async fn serve(
    mint: Arc<Mint>,
    market: Router,
    listen: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // The market rides on the same listener as the mint under its own path.
    // It is a separate protocol that happens to be served by the same process;
    // a node may point its peers at somebody else's market entirely. All that
    // is served here is the price signal — selling this node's own vouchers is
    // the mint API beside it, and needs nothing of its own.
    let router = cdk_axum::create_mint_router(Arc::clone(&mint), vec![])
        .await
        .context("build the mint router")?
        .merge(market);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind the mint on {listen}"))?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serve the mint")?;

    mint.stop().await.context("stop the mint")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_unit_is_a_custom_keyset_unit() {
        // Cashu has no byte unit of its own; it is the resource that fixes it.
        assert_eq!(
            currency_unit("byte"),
            CurrencyUnit::Custom("byte".into())
        );
    }

    #[test]
    fn sat_stays_the_cashu_sat() {
        // Otherwise a sat-denominated node would be issuing something ordinary
        // wallets could not read.
        assert_eq!(currency_unit("sat"), CurrencyUnit::Sat);
    }

    #[tokio::test]
    async fn a_byte_denominated_mint_comes_up_with_an_active_keyset() {
        let mint = build(&MintConfig {
            url: "http://127.0.0.1:0".into(),
            unit: "byte".into(),
            seed: vec![7; 32],
            max_amount: 1_000_000_000,
        })
        .await
        .expect("build a byte mint");

        assert!(
            mint.get_active_keysets()
                .contains_key(&CurrencyUnit::Custom("byte".into())),
            "the byte keyset should be active"
        );
    }
}
