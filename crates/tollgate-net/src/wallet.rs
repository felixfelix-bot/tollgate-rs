//! What this node is holding, and how it spends it.
//!
//! A thin layer over cdk's wallet, and deliberately nothing more. Proof
//! selection, change, keyset rotation, the spent-proof bookkeeping and the
//! database behind it are all Cashu problems that cdk has already solved; what
//! this adds is the part that is TollGate's — a node holds paper from several
//! issuers, and what it can do with a holding depends on who issued it.
//!
//! - **money** — sat-denominated tokens from a mint that deals in money. Any
//!   peer that accepts that mint will take them, so this is what a node pays
//!   with generally, and the only thing it can top itself up in.
//! - **prepaid transit** — byte-denominated tokens from an *upstream*: capacity
//!   bought and not yet spent. Also spending power, but only at the one node
//!   that issued them, since nobody else's mint honours them.
//!
//! What a node never accumulates is its own vouchers. A customer pays in the
//! paper this node issued, and redeeming that cancels this node's own claim —
//! the money side of being paid arrives as money, at the market, which is why
//! selling shows up in the first list and not the second.
//!
//! So one cdk wallet per (mint, unit), created on demand and kept in one
//! database. Balances are reported money first, because a node short of money
//! cannot buy from anyone, where a node short of one upstream's vouchers is
//! only short with that upstream.
//!
//! # Why the node holds anything at all
//!
//! It could buy money for each purchase and hold nothing. That is what it did
//! first, and it is wrong twice over: every channel it funds then waits on a
//! Lightning settlement — seconds, inside a rollover that has milliseconds —
//! and the money it *takes* has nowhere to go. A balance answers both. Buying
//! becomes local, and payment received is payment kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cdk::amount::SplitTarget;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod, Token};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet as CdkWallet};
use cdk_common::database::WalletDatabase;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::mint::currency_unit;

/// What a holding can be spent on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Money. Any peer that accepts the issuing mint will take it, so it buys
    /// from whoever this node decides to buy from.
    Money,
    /// Capacity bought from one upstream and not yet spent. Spending power at
    /// that node and nowhere else — nobody else's mint honours its paper.
    PrepaidTransit,
}

impl Kind {
    /// What a unit means to a node that sells `resource`.
    ///
    /// Anything the node does not itself sell is money to it; its own resource
    /// unit, issued by somebody else, is a prepayment to that somebody.
    pub fn of(unit: &str, resource: &str) -> Self {
        if unit == resource {
            Kind::PrepaidTransit
        } else {
            Kind::Money
        }
    }
}

/// One issuer's paper, in one unit, and how much of it there is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    /// The mint that issued it.
    pub mint: String,
    /// What it is denominated in — `sat` for money, `byte` for capacity.
    pub unit: String,
    /// How much, in that unit.
    pub amount: u64,
    /// What it can be spent on, and with whom.
    ///
    /// Stated rather than left for each reader to infer: both kinds are
    /// spending power, and the difference is whether it is general or is good
    /// at exactly one node.
    pub kind: Kind,
}

/// A top-up waiting to be paid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUp {
    /// The mint selling the money.
    pub mint: String,
    /// What it is denominated in.
    pub unit: String,
    /// How much was asked for.
    pub amount: u64,
    /// The mint's quote id, to claim it with once it is paid.
    pub quote: String,
    /// The bolt11 invoice to pay.
    pub request: String,
}

/// Everything this node holds, across issuers and units.
#[derive(Clone)]
pub struct Wallet {
    /// One cdk wallet per (mint, unit). Created on demand: a node learns which
    /// issuers it holds paper from by being paid in it.
    wallets: Arc<Mutex<BTreeMap<(String, String), CdkWallet>>>,
    store: Arc<dyn WalletDatabase<cdk_common::database::Error> + Send + Sync>,
    /// Derives the wallet's keys. The node's own identity, hashed, so a
    /// restored config restores the balance with it.
    seed: [u8; 64],
    /// The unit this node sells. Holdings in it are somebody else's capacity;
    /// holdings in anything else are money.
    resource: String,
    path: PathBuf,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Open the wallet database at `path`, creating it if it is not there.
    ///
    /// The seed is derived from the node's own secret, so the wallet is part of
    /// the node's identity rather than a separate thing to back up: restore the
    /// config on a new box and the proofs it holds are recoverable from it.
    ///
    /// `resource` is the unit this node sells, which is what tells a holding in
    /// it apart from money.
    pub async fn open(
        path: impl Into<PathBuf>,
        seed: [u8; 64],
        resource: impl Into<String>,
    ) -> Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }

        let store = cdk_sqlite::wallet::WalletSqliteDatabase::new(&path)
            .await
            .with_context(|| format!("open the wallet at {}", path.display()))?;

        Ok(Self {
            wallets: Arc::new(Mutex::new(BTreeMap::new())),
            store: Arc::new(store),
            seed,
            resource: resource.into(),
            path,
        })
    }

    /// Where the wallet lives, for anything that has to say so.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cdk wallet for one issuer's paper, made if this is the first time.
    async fn wallet_for(&self, mint: &str, unit: &str) -> Result<CdkWallet> {
        let key = (normalise(mint), unit.to_string());
        let mut wallets = self.wallets.lock().await;
        if let Some(wallet) = wallets.get(&key) {
            return Ok(wallet.clone());
        }

        let wallet = CdkWallet::new(
            &key.0,
            currency_unit(unit),
            Arc::clone(&self.store),
            self.seed,
            None,
        )
        .map_err(|e| anyhow!("open a wallet for {unit} at {mint}: {e}"))?;
        wallets.insert(key, wallet.clone());
        Ok(wallet)
    }

    /// Take a token in and hold it.
    ///
    /// Receiving is a swap at the issuing mint, so this both proves the paper
    /// was good and moves it under keys only this node has.
    pub async fn deposit(&self, token: &str) -> Result<Holding> {
        let (mint, unit, _) = describe(token)?;
        let wallet = self.wallet_for(&mint, &unit).await?;
        let amount: u64 = wallet
            .receive(token.trim(), ReceiveOptions::default())
            .await
            .map_err(|e| anyhow!("receive {unit} from {mint}: {e}"))?
            .into();

        info!(%mint, %unit, amount, "deposited");
        Ok(Holding {
            mint,
            kind: Kind::of(&unit, &self.resource),
            unit,
            amount,
        })
    }

    /// Take exactly `amount` of one issuer's paper out, as a token.
    ///
    /// cdk decides which proofs go and swaps for change where it has to; what
    /// comes back is a token for exactly the amount asked for, or an error and
    /// an untouched balance.
    pub async fn spend(&self, mint: &str, unit: &str, amount: u64) -> Result<String> {
        if amount == 0 {
            bail!("spending nothing is not a payment");
        }
        let wallet = self.wallet_for(mint, unit).await?;
        let prepared = wallet
            .prepare_send(amount.into(), SendOptions::default())
            .await
            .map_err(|e| anyhow!("this node cannot pay {amount} {unit} from {mint}: {e}"))?;
        let token = prepared
            .confirm(None)
            .await
            .map_err(|e| anyhow!("send {amount} {unit} from {mint}: {e}"))?;

        debug!(%mint, %unit, amount, "spent");
        Ok(token.to_string())
    }

    /// What is held, money first.
    ///
    /// Money before prepaid transit because a node short of money cannot buy
    /// from anybody, where a node short of one upstream's vouchers is only
    /// short with that upstream.
    pub async fn balances(&self) -> Vec<Holding> {
        let wallets: Vec<((String, String), CdkWallet)> = self
            .wallets
            .lock()
            .await
            .iter()
            .map(|(k, w)| (k.clone(), w.clone()))
            .collect();

        let mut holdings = Vec::new();
        for ((mint, unit), wallet) in wallets {
            let amount: u64 = wallet.total_balance().await.unwrap_or_default().into();
            if amount == 0 {
                continue;
            }
            holdings.push(Holding {
                mint,
                kind: Kind::of(&unit, &self.resource),
                unit,
                amount,
            });
        }

        holdings.sort_by_key(|h| (h.kind != Kind::Money, h.unit.clone(), h.mint.clone()));
        holdings
    }

    /// How much of one issuer's paper is held.
    pub async fn balance_of(&self, mint: &str, unit: &str) -> u64 {
        match self.wallet_for(mint, unit).await {
            Ok(wallet) => wallet.total_balance().await.unwrap_or_default().into(),
            Err(_) => 0,
        }
    }

    /// Buy money at a mint that sells it, and return the invoice to pay.
    ///
    /// Nothing is held until it is paid, which is [`Wallet::collect`]'s job:
    /// somebody paying a Lightning invoice takes as long as they take, and the
    /// node has other things to do meanwhile.
    pub async fn top_up(&self, mint: &str, unit: &str, amount: u64) -> Result<TopUp> {
        if amount == 0 {
            bail!("topping up by nothing is not a top-up");
        }
        let wallet = self.wallet_for(mint, unit).await?;
        let quote = wallet
            .mint_quote(
                PaymentMethod::Known(KnownMethod::Bolt11),
                Some(amount.into()),
                None,
                None,
            )
            .await
            .map_err(|e| anyhow!("ask {mint} for {amount} {unit}: {e}"))?;

        info!(%mint, %unit, amount, "waiting on payment for a top-up");
        Ok(TopUp {
            mint: normalise(mint),
            unit: unit.to_string(),
            amount,
            quote: quote.id.to_string(),
            request: quote.request.clone(),
        })
    }

    /// Claim a top-up once its invoice has been paid.
    ///
    /// Returns what was actually minted, which is not always what was asked
    /// for: a quote can be paid short, and the mint is the authority on what it
    /// received.
    pub async fn collect(&self, top_up: &TopUp) -> Result<u64> {
        let wallet = self.wallet_for(&top_up.mint, &top_up.unit).await?;
        let proofs = wallet
            .mint(&top_up.quote, SplitTarget::default(), None)
            .await
            .map_err(|e| {
                anyhow!(
                    "claim {} {} from {}: {e}",
                    top_up.amount,
                    top_up.unit,
                    top_up.mint
                )
            })?;

        let minted: u64 = proofs
            .iter()
            .map(|p| u64::from(p.amount))
            .fold(0, u64::saturating_add);
        info!(mint = %top_up.mint, unit = %top_up.unit, minted, "topped up");
        Ok(minted)
    }

    /// Finish or roll back anything a previous run left half-done.
    ///
    /// cdk drives minting and sending as sagas, and a saga interrupted mid-way
    /// leaves its quote or its proofs reserved — the next attempt is refused
    /// with "already in use by another operation". A node is killed mid-mint by
    /// an ordinary restart, so this is not an edge case; cdk expects it to be
    /// called at startup and we were not calling it.
    ///
    /// Needs the mint to be reachable, so a failure here is not fatal: it is
    /// retried the next time anything claims.
    async fn recover(&self, mint: &str, unit: &str) -> Result<()> {
        let wallet = self.wallet_for(mint, unit).await?;
        match wallet.recover_incomplete_sagas().await {
            Ok(report) => {
                if report.recovered + report.compensated + report.failed > 0 {
                    info!(
                        %mint,
                        recovered = report.recovered,
                        rolled_back = report.compensated,
                        failed = report.failed,
                        "finished what an earlier run left open"
                    );
                }
                Ok(())
            }
            Err(e) => {
                debug!(%mint, error = %e, "could not recover incomplete operations");
                Ok(())
            }
        }
    }

    /// Claim everything paid for and not yet collected.
    ///
    /// A quote lives in the wallet's own database from the moment it is asked
    /// for, so an invoice paid while the node was restarting, or while the mint
    /// was rate-limiting the status polls, is money waiting rather than money
    /// lost. This is what goes and gets it.
    ///
    /// Quotes that are simply unpaid fail here and stay pending, which is why
    /// nothing is removed and the count is of what was actually minted.
    pub async fn claim_pending(&self, mint: &str, unit: &str) -> Result<u64> {
        // A quote a killed run left reserved cannot be minted until the saga
        // that reserved it is settled, and that is exactly the state a claim is
        // called in.
        self.recover(mint, unit).await?;

        let wallet = self.wallet_for(mint, unit).await?;
        let pending = wallet
            .get_unissued_mint_quotes()
            .await
            .map_err(|e| anyhow!("list unclaimed quotes at {mint}: {e}"))?;

        if pending.is_empty() {
            debug!(%mint, %unit, "nothing outstanding to claim");
            return Ok(0);
        }

        debug!(%mint, %unit, outstanding = pending.len(), "claiming");

        let mut claimed = 0u64;
        for (i, quote) in pending.iter().enumerate() {
            // Paced. Every attempt is a request to somebody else's server, and
            // a node that has been running a while has a quote per unpaid
            // top-up: claiming forty of them as fast as the loop goes is how a
            // mint starts answering "rate limit exceeded" to everything, which
            // is the failure this whole path exists to recover from.
            if i > 0 {
                tokio::time::sleep(BETWEEN_CLAIMS).await;
            }

            match wallet
                .mint(&quote.id.to_string(), SplitTarget::default(), None)
                .await
            {
                Ok(proofs) => {
                    let minted: u64 = proofs
                        .iter()
                        .map(|p| u64::from(p.amount))
                        .fold(0, u64::saturating_add);
                    claimed = claimed.saturating_add(minted);
                    info!(%mint, %unit, minted, quote = %quote.id, "claimed");
                }
                // Usually "not paid yet", which is not a fault: the quote stays
                // in the database and the next attempt tries again.
                Err(e) => debug!(%mint, quote = %quote.id, error = %e, "not claimable yet"),
            }
        }
        Ok(claimed)
    }
}

/// How long to leave between claim attempts against one mint.
const BETWEEN_CLAIMS: std::time::Duration = std::time::Duration::from_millis(400);

fn normalise(mint_url: &str) -> String {
    mint_url.trim_end_matches('/').to_string()
}

/// A token's issuer, unit and value.
fn describe(token: &str) -> Result<(String, String, u64)> {
    let parsed = Token::from_str(token.trim()).map_err(|e| anyhow!("parse a token: {e}"))?;
    let mint = parsed
        .mint_url()
        .map_err(|e| anyhow!("read a token's mint: {e}"))?
        .to_string();
    let unit = parsed.unit().unwrap_or(CurrencyUnit::Sat).to_string();
    let amount: u64 = parsed
        .value()
        .map_err(|e| anyhow!("read a token's value: {e}"))?
        .into();
    Ok((normalise(&mint), unit, amount))
}

/// Where a node keeps its wallet unless told otherwise.
///
/// Beside the state a service manager already owns, so the packages keep it
/// across an upgrade without a special case.
pub fn default_path() -> PathBuf {
    for candidate in ["/var/lib/tollgate", "/usr/local/var/lib/tollgate"] {
        let dir = Path::new(candidate);
        if dir.is_dir() || std::fs::create_dir_all(dir).is_ok() {
            return dir.join("wallet.sqlite");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(format!("{xdg}/tollgate/wallet.sqlite"));
    }
    PathBuf::from("/tmp/tollgate-wallet.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wallet in a directory that removes itself.
    async fn wallet() -> (Wallet, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        let wallet = Wallet::open(dir.path().join("wallet.sqlite"), [7u8; 64], "byte")
            .await
            .expect("open");
        (wallet, dir)
    }

    #[tokio::test]
    async fn a_new_wallet_holds_nothing() {
        let (wallet, _dir) = wallet().await;
        assert!(wallet.balances().await.is_empty());
        assert_eq!(wallet.balance_of("https://mint.example", "sat").await, 0);
    }

    #[tokio::test]
    async fn spending_what_is_not_held_fails() {
        let (wallet, _dir) = wallet().await;
        assert!(
            wallet
                .spend("https://mint.example", "sat", 10)
                .await
                .is_err()
        );
        assert!(
            wallet
                .spend("https://mint.example", "sat", 0)
                .await
                .is_err(),
            "spending nothing is not a payment"
        );
        assert!(wallet.balances().await.is_empty());
    }

    #[tokio::test]
    async fn topping_up_by_nothing_is_refused() {
        let (wallet, _dir) = wallet().await;
        assert!(
            wallet
                .top_up("https://mint.example", "sat", 0)
                .await
                .is_err()
        );
    }

    #[test]
    fn anything_but_the_resource_is_money() {
        // Money buys from whoever accepts its mint. Paper denominated in the
        // thing this node sells is somebody else's capacity, bought and not yet
        // spent, and only that somebody honours it.
        assert_eq!(Kind::of("sat", "byte"), Kind::Money);
        assert_eq!(Kind::of("usd", "byte"), Kind::Money);
        assert_eq!(Kind::of("byte", "byte"), Kind::PrepaidTransit);

        // And a node selling something else reads the same units differently:
        // to one selling watt-hours, a byte voucher is money like any other.
        assert_eq!(Kind::of("byte", "watt_hour"), Kind::Money);
    }

    #[test]
    fn a_node_never_holds_its_own_vouchers() {
        // Worth stating because the wallet's shape assumes it: a customer pays
        // in the paper this node issued, and redeeming that cancels this node's
        // own claim rather than adding to a balance. What selling puts in the
        // wallet is money, deposited at the market.
        //
        // So every `PrepaidTransit` holding is by construction some *other*
        // mint's, and there is no case where the issuer is this node.
        assert_eq!(Kind::of("byte", "byte"), Kind::PrepaidTransit);
    }

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "tollgate-wallet-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).expect("create a temporary directory");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
