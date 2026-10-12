//! Real Cashu Spilman channels, funded against a real mint.
//!
//! This is the backend that puts value at stake. A channel is a 2-of-2 multisig
//! Cashu token; a ratchet turn is a signed balance update the receiver can take
//! to the mint; settlement is a swap.
//!
//! # No prices here
//!
//! The receiver is configured with a pricing entry that **prices nothing**, so
//! the Spilman layer's "amount due" is always zero and it enforces only the
//! ratchet and the signature. That is deliberate and it is the whole fit: delivery has no price
//! ([`tollgate-vouchers.md`]), and what a peer may draw is decided by
//! `tollgate-core`'s admission control against the grant. The channel layer
//! carries the money; it does not decide how much is owed.
//!
//! # Where the vouchers come from
//!
//! A channel is funded in a mint the **peer** accepts, and the peer's own mint
//! is always reachable — it is the peer we are already talking to. So funding a
//! channel to pay a peer means first holding that peer's vouchers, and how a
//! peer came to hold vouchers is not the protocol's business, any more than how
//! it came to hold sats. Buying them is [`crate::market`]'s job, and this layer
//! only asks it for what a channel needs.
//!
//! [`tollgate-vouchers.md`]: https://github.com/OpenTollGate/tollgate-rs/blob/master/docs/design/core/tollgate-vouchers.md

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use cdk::mint::Mint;
use cdk_spilman::configurable_host::{
    ConfigurableHost, ConfigurableHostConfig, KeysetCacheEntry, StorageConfig, UnitPricingConfig,
};
use cdk_spilman::{
    ConfigurableClientHost, MemoryClientStorage, ReqwestClientNetworking, SpilmanBridge,
    SpilmanClientBridge, SpilmanKeysetRefresher, SpilmanMintClient,
};
use tollgate_protocol::{ChannelId, PubKey, Signature};
use tracing::debug;

use super::{ChannelBackend, FundedChannel, VerifiedChannel};

/// Shortest expiry we will accept on a channel a peer funds to pay us.
///
/// The refund timelock is what a payer would rely on if the receiver vanished,
/// so a receiver insisting on a floor is insisting the payer keep its own
/// protection.
const MIN_EXPIRY_SECONDS: u64 = 3_600;

/// How long a channel *we* fund stays refundable to us.
///
/// Comfortably longer than the floor we ourselves require, because time passes
/// between choosing the expiry and the peer checking it: the vouchers have to
/// be acquired, the channel funded, the Accept sent and the funding verified.
/// Funding at exactly the minimum means every one of those seconds counts
/// against us, and the peer refuses a channel that was valid when it was built.
const CHANNEL_TTL_SECONDS: u64 = 2 * MIN_EXPIRY_SECONDS;

/// Largest single proof amount used when funding.
///
/// Amounts are powers of two, so a channel takes one proof per set bit and the
/// count is what the funding blob and the spent-proof set both scale with. Byte
/// denomination makes the numbers large — a 1 GiB channel is a 30-bit number,
/// so ~30 proofs at worst and about 15 on average.
///
/// Capping this low would be ruinous here: at 8192 the same channel would need
/// 131,072 proofs. The cap exists for money-denominated channels where a small
/// maximum keeps individual proofs cheap; for bytes it has to track the
/// capacity being funded.
const MAX_PROOF_AMOUNT: u64 = 1 << 30;

/// How long one request to a peer's mint may take.
///
/// These calls sit inline in a purchase, so a mint that hangs would otherwise
/// stall buying until the operating system gave up on the socket.
const MINT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What a Spilman backend needs to know about this node.
#[derive(Debug, Clone)]
pub struct SpilmanConfig {
    /// This node's own mint, which its peers fund channels against.
    pub mint: Arc<Mint>,
    /// The URL peers reach that mint on, as advertised in our Offer.
    pub mint_url: String,
    /// The unit our keyset denominates in.
    pub unit: String,
    /// Mints we will take payment in, from the Offer. A channel funded against
    /// anything else is refused.
    pub accepted_mints: Vec<String>,
    /// Secret key this node signs channel state with, hex-encoded.
    pub secret_key_hex: String,
    /// What this node holds, and what it pays peers out of.
    pub wallet: crate::wallet::Wallet,
    /// Which of its holdings is the money — the paper a peer has to accept
    /// before this node can pay it. `None` for a node that only sells.
    pub money: Option<Money>,
}

/// Where a node's money is, and in what.
#[derive(Debug, Clone)]
pub struct Money {
    /// A mint that sells its paper for money.
    pub mint: String,
    /// The unit that mint's paper is denominated in.
    pub unit: String,
}

type Client =
    SpilmanClientBridge<ConfigurableClientHost<MemoryClientStorage>, ReqwestClientNetworking>;

/// Cashu Spilman channels.
pub struct SpilmanChannels {
    config: SpilmanConfig,
    /// The wallet is async and this backend is not, so its calls are handed
    /// back to the runtime rather than run on a worker.
    runtime: tokio::runtime::Handle,
    /// Our side of channels we fund to pay peers.
    ///
    /// Behind a mutex because the client host keeps its storage in a `RefCell`
    /// — `Send` but not `Sync`, so the lock is what makes the backend shareable
    /// rather than a concession to contention.
    client: Mutex<Client>,
    /// Our side of channels peers fund to pay us.
    server: SpilmanBridge<ConfigurableHost>,
}

impl std::fmt::Debug for SpilmanChannels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpilmanChannels")
            .field("mint_url", &self.config.mint_url)
            .field("unit", &self.config.unit)
            .finish_non_exhaustive()
    }
}

impl SpilmanChannels {
    /// Build a backend around this node's mint and key.
    ///
    /// Must be called from inside a tokio runtime: the client networking binds
    /// the current handle so it can drive HTTP from the synchronous trait.
    pub fn new(config: SpilmanConfig) -> Result<Self> {
        let secret = cashu::nuts::SecretKey::from_hex(&config.secret_key_hex)
            .map_err(|e| anyhow!("channel secret key is not valid: {e}"))?;

        let mut client_host = ConfigurableClientHost::new_in_memory();
        client_host.add_key(secret);
        let networking = ReqwestClientNetworking::new(MINT_REQUEST_TIMEOUT)
            .map_err(|e| anyhow!("build the mint client: {e}"))?;
        let client = SpilmanClientBridge::new(client_host, networking);

        // Every mint we accept, trusted for our unit. Accept or refuse is
        // binary — there is no haircut, because what an issuer's paper is worth
        // is expressed in what you pay for it on the market, not in a discount
        // applied at settlement.
        let mints = config
            .accepted_mints
            .iter()
            .map(|url| (url.clone(), vec![config.unit.clone()]))
            .collect();

        let host = ConfigurableHost::new(
            ConfigurableHostConfig {
                mints,
                min_expiry_seconds: MIN_EXPIRY_SECONDS,
                pricing_scale: 1,
                storage: StorageConfig::Memory,
                // A pricing entry with **nothing priced**. The amount due is a
                // linear combination over priced variables, so an empty set
                // makes it zero always: this layer enforces the ratchet and the
                // signature and never second-guesses what is owed. What a peer
                // may draw is core's decision, against the grant.
                //
                // The entry has to exist even though it prices nothing — the
                // host refuses to start if a unit is trusted by a mint but
                // absent from the table.
                pricing: HashMap::from([(
                    config.unit.clone(),
                    UnitPricingConfig {
                        min_capacity: 1,
                        max_amount_per_output: Some(MAX_PROOF_AMOUNT),
                        variables: HashMap::new(),
                    },
                )]),
            },
            &config.secret_key_hex,
        )
        .map_err(|e| anyhow!("build the channel receiver: {e}"))?;

        Ok(Self {
            config,
            runtime: tokio::runtime::Handle::current(),
            client: Mutex::new(client),
            server: SpilmanBridge::new(host),
        })
    }

    /// The mint URL this node advertises.
    pub fn mint_url(&self) -> &str {
        &self.config.mint_url
    }

    /// Make sure we hold the keys for the keyset a peer funded against.
    ///
    /// The receiver will not accept a channel on a keyset it cannot verify
    /// signatures against, and there is no way to know ahead of time which
    /// keyset of which accepted mint a peer will choose.
    fn cache_keyset(&self, params: &serde_json::Value) -> Result<()> {
        let mint = params["mint"]
            .as_str()
            .ok_or_else(|| anyhow!("channel parameters name no mint"))?;
        let keyset_id = params["keyset_id"]
            .as_str()
            .or_else(|| params["keyset_info"]["keysetId"].as_str())
            .ok_or_else(|| anyhow!("channel parameters name no keyset"))?;

        if !self.config.accepted_mints.iter().any(|m| m == mint) {
            bail!("{mint} is not a mint we take payment in");
        }

        let id: cashu::nuts::Id = keyset_id
            .parse()
            .map_err(|e| anyhow!("keyset id {keyset_id:?}: {e}"))?;

        let info = self
            .client
            .lock()
            .expect("not poisoned")
            .fetch_keyset_info(mint, keyset_id)
            .map_err(|e| anyhow!("read keyset {keyset_id} from {mint}: {e}"))?;

        self.server
            .host()
            .set_keyset(
                mint,
                id,
                KeysetCacheEntry {
                    info_json: info,
                    active: true,
                    unit: crate::mint::currency_unit(&self.config.unit),
                },
            )
            .map_err(|e| anyhow!("cache keyset {keyset_id}: {e}"))?;
        Ok(())
    }
}

/// Seconds since the epoch, for the channel's refund timelock.
///
/// The only wall-clock read in the crate. Everything the protocol measures is
/// relative and comes from the host's monotonic clock.
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn channel_id_from_hex(s: &str) -> Result<ChannelId> {
    let bytes = hex::decode(s).with_context(|| format!("channel id {s:?} is not hex"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("channel id {s:?} is not 32 bytes"))?;
    Ok(ChannelId(bytes))
}

fn channel_id_to_hex(id: ChannelId) -> String {
    hex::encode(id.0)
}

fn signature_from_hex(s: &str) -> Result<Signature> {
    let bytes = hex::decode(s).context("signature is not hex")?;
    let bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("signature is not 64 bytes"))?;
    Ok(Signature(bytes))
}

/// What travels in `Accept` and `RolloverInit`.
///
/// Spilman carries funding with the first payment; the TollGate handshake
/// confirms a channel before any grant, so the opening payment is pulled out
/// and sent here instead. Its balance is zero — it buys nothing, it only opens.
#[derive(serde::Serialize, serde::Deserialize)]
struct FundingBlob {
    channel_id: String,
    balance: u64,
    signature: String,
    params: serde_json::Value,
    funding_proofs: serde_json::Value,
}

impl SpilmanChannels {
    /// Buy `capacity` of a peer's vouchers, paying at its market.
    ///
    /// Three steps and no negotiation: ask what the peer takes, turn money into
    /// that issuer's paper, and hand it over for vouchers. The peer prices the
    /// trade; this side only decides whether to accept the price by going
    /// through with it.
    fn buy_vouchers(&self, mint_url: &str, capacity: u64, keyset_info: &str) -> Result<String> {
        let Some(money) = &self.config.money else {
            bail!(
                "this node holds no money, so it cannot buy the vouchers it would pay {mint_url} with"
            );
        };

        // The peer's market is served beside its mint, at the URL it already
        // advertises.
        let market = crate::market::info_of(mint_url)?;
        let price = market
            .accepts
            .iter()
            .find(|a| {
                a.mint.trim_end_matches('/') == money.mint.trim_end_matches('/')
                    && a.unit == money.unit
            })
            .ok_or_else(|| {
                anyhow!(
                    "{mint_url} does not take {} from {}; it takes {:?}",
                    money.unit,
                    money.mint,
                    market
                        .accepts
                        .iter()
                        .map(|a| format!("{} {}", a.unit, a.mint))
                        .collect::<Vec<_>>()
                )
            })?;

        // Rounded up, so a purchase is never short of what it asked for.
        let owed = (capacity as u128)
            .div_ceil(price.bytes_per_unit as u128)
            .max(1) as u64;
        debug!(
            capacity,
            owed,
            unit = %money.unit,
            mint = %money.mint,
            "buying vouchers"
        );

        let payment = self.take_from_wallet(money, owed)?;
        crate::market::buy(mint_url, capacity, &self.config.unit, keyset_info, &payment)
    }

    /// Take `owed` out of the wallet, topping it up if it is short.
    ///
    /// Spending what is already held is the normal path and is local
    /// arithmetic. The top-up is the exception, and it is deliberately in-line
    /// rather than a background chore: a node that has run out of money has
    /// stopped being able to buy transit, and the operator wants that to show
    /// up as a slow purchase rather than a silent one.
    ///
    /// Where the money mint settles its own invoices this is invisible. Where a
    /// human has to pay one, it fails with the invoice in the log, and the
    /// operator tops up from `tolltop` instead.
    fn take_from_wallet(&self, money: &Money, owed: u64) -> Result<String> {
        let wallet = self.config.wallet.clone();
        let handle = self.runtime.clone();

        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                if let Ok(token) = wallet.spend(&money.mint, &money.unit, owed).await {
                    return Ok(token);
                }

                let held = wallet.balance_of(&money.mint, &money.unit).await;
                debug!(held, owed, "topping up to cover a purchase");
                let top_up = wallet
                    .top_up(
                        &money.mint,
                        &money.unit,
                        owed.saturating_sub(held).max(owed),
                    )
                    .await?;
                crate::market::wait_until_paid(&money.mint, &top_up.quote)
                    .with_context(|| format!("pay {}", top_up.request))?;
                wallet.collect(&top_up).await?;

                wallet
                    .spend(&money.mint, &money.unit, owed)
                    .await
                    .with_context(|| format!("pay {owed} {} after topping up", money.unit))
            })
        })
    }
}

impl ChannelBackend for SpilmanChannels {
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel> {
        // Which keyset denominates in our unit. A mint may run several — an old
        // one still being redeemed alongside the active one — and only the
        // active one can be funded against.
        let keyset_id = crate::market::active_keyset(mint_url, &self.config.unit)?;
        let keyset_info = self
            .client
            .lock()
            .expect("not poisoned")
            .fetch_keyset_info(mint_url, &keyset_id)
            .map_err(|e| anyhow!("could not read keyset {keyset_id} from {mint_url}: {e}"))?;

        // Buying the peer's vouchers is outside the protocol: a peer arrives
        // holding them or it gets no service, and how it came to hold them is
        // as much its own business as how it came to hold money.
        let token = self
            .buy_vouchers(mint_url, capacity, &keyset_info)
            .with_context(|| format!("buy {capacity} {} from {mint_url}", self.config.unit))?;

        let ours = cashu::nuts::SecretKey::from_hex(&self.config.secret_key_hex)
            .map_err(|e| anyhow!("bad secret: {e}"))?
            .public_key()
            .to_hex();

        let client = self.client.lock().expect("not poisoned");
        let opened = client
            .open_channel_from_token(
                &token,
                &hex::encode(peer.0),
                &ours,
                now_seconds() + CHANNEL_TTL_SECONDS,
                &keyset_info,
                MAX_PROOF_AMOUNT,
            )
            .map_err(|e| anyhow!("open a channel against {mint_url}: {e}"))?;

        let opening = client
            .sign_channel_registration(&opened.channel_id)
            .map_err(|e| anyhow!("prepare funding for {}: {e}", opened.channel_id))?;

        let blob = FundingBlob {
            channel_id: opened.channel_id.clone(),
            balance: opening.balance,
            signature: opening.signature.clone(),
            params: opening
                .params
                .clone()
                .ok_or_else(|| anyhow!("the opening payment carried no channel parameters"))?,
            funding_proofs: serde_json::to_value(
                opening
                    .funding_proofs
                    .as_deref()
                    .ok_or_else(|| anyhow!("the opening payment carried no proofs"))?,
            )?,
        };

        Ok(FundedChannel {
            channel_id: channel_id_from_hex(&opened.channel_id)?,
            capacity: opened.capacity,
            funding: serde_json::to_vec(&blob)?,
        })
    }

    fn verify(&self, _peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel> {
        let blob: FundingBlob =
            serde_json::from_slice(funding).context("funding blob is not the expected shape")?;
        let proofs: Vec<cashu::nuts::Proof> = serde_json::from_value(blob.funding_proofs)
            .context("funding blob carried unreadable proofs")?;

        // A keyset is only acceptable once we hold its keys, and we cannot know
        // in advance which of our accepted mints a peer will fund against — or
        // which keyset that mint had active at the time. So it is fetched when
        // the funding names it, from the mint that issued it.
        self.cache_keyset(&blob.params)?;

        // This is where the money is checked: locked to the two of us, against
        // a mint and keyset we accept, and not already spent.
        let funded = self
            .server
            .fund_channel(
                &blob.channel_id,
                blob.balance,
                &blob.signature,
                Some(&blob.params),
                Some(&proofs),
            )
            .map_err(|e| anyhow!("peer funding did not verify: {e:?}"))?;

        Ok(VerifiedChannel {
            channel_id: channel_id_from_hex(&blob.channel_id)?,
            capacity: funded.capacity,
        })
    }

    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature> {
        let payment = self
            .client
            .lock()
            .expect("not poisoned")
            .sign_and_record_payment(&channel_id_to_hex(channel_id), cumulative)
            .map_err(|e| anyhow!("sign a channel update: {e}"))?;
        signature_from_hex(&payment.signature)
    }

    fn verify_update(
        &self,
        _peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool {
        // Checks the signature against the channel's key material and that the
        // balance only ever increases. With no pricing configured it never
        // second-guesses how much is owed — that is core's job.
        self.server
            .process_payment(
                &channel_id_to_hex(channel_id),
                cumulative,
                &hex::encode(signature.0),
                None,
                None,
                &String::new(),
            )
            .is_ok()
    }

    fn settle(&self, channel_id: ChannelId) -> Result<()> {
        let id = channel_id_to_hex(channel_id);
        let networking = MintNetworking::new(Arc::clone(&self.config.mint));
        self.server
            .execute_unilateral_close(&id, &networking, &networking)
            .map_err(|e| anyhow!("settle {id}: {e:?}"))?;
        Ok(())
    }
}

/// Talks to a mint that is running in this process.
///
/// Settling our own vouchers cancels our own claim, so the swap never leaves
/// the node — no HTTP, and it works during an upstream outage, which is the
/// single largest resilience gain of the voucher model.
struct MintNetworking {
    mint: Arc<Mint>,
    runtime: tokio::runtime::Handle,
}

impl MintNetworking {
    fn new(mint: Arc<Mint>) -> Self {
        Self {
            mint,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl SpilmanMintClient for MintNetworking {
    fn call_mint_swap(&self, _mint_url: &str, swap_request_json: &str) -> Result<String, String> {
        let request: cashu::nuts::SwapRequest =
            serde_json::from_str(swap_request_json).map_err(|e| e.to_string())?;

        let mint = Arc::clone(&self.mint);
        let handle = self.runtime.clone();
        // The backend trait is synchronous and the node already drives it from
        // a blocking thread, so this hands the future back to the runtime
        // rather than blocking a worker.
        let response = tokio::task::block_in_place(|| {
            handle.block_on(async move { mint.process_swap_request(request).await })
        })
        .map_err(|e| e.to_string())?;

        serde_json::to_string(&response).map_err(|e| e.to_string())
    }
}

impl SpilmanKeysetRefresher for MintNetworking {
    fn refresh(&self, _mint: &str) -> Result<(), String> {
        // Ours, and always current.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_id_round_trips_through_hex() {
        let id = ChannelId([7; 32]);
        assert_eq!(
            channel_id_from_hex(&channel_id_to_hex(id)).expect("parse"),
            id
        );
    }

    #[test]
    fn a_channel_id_of_the_wrong_length_is_rejected() {
        assert!(channel_id_from_hex(&hex::encode([0u8; 16])).is_err());
        assert!(channel_id_from_hex("nonsense").is_err());
    }

    #[test]
    fn a_signature_of_the_wrong_length_is_rejected() {
        assert!(signature_from_hex(&hex::encode([0u8; 64])).is_ok());
        assert!(signature_from_hex(&hex::encode([0u8; 32])).is_err());
    }
}
