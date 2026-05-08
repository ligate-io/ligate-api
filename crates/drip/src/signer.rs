//! Hot-key signer for ligate-faucet.
//!
//! Signs and submits a `bank.transfer` to drip `$LGT` to the
//! requested address. Uses [`ligate_client::submit::Submitter`] for
//! chain interaction.
//!
//! ## Wire format (for context)
//!
//! 1. Build `RuntimeCall::Bank(CallMessage::Transfer { to, coins })`
//!    against the chain's runtime composition.
//! 2. Wrap in `UnsignedTransaction::new` with chain id, nonce, fees.
//! 3. Sign: `unsigned.sign(&private_key, &chain_hash)` returns a
//!    `Transaction`. The signature binds to `chain_hash` so the same
//!    private key produces a different signature on each chain id.
//! 4. Borsh-encode the signed transaction. The chain's
//!    `POST /v1/sequencer/txs` handler wraps the body in
//!    `AuthenticatorInput::Standard(RawTx { data })` server-side, so
//!    we do NOT pre-wrap on the client. (Doing so double-wraps and
//!    the chain rejects with `Cannot decompress Edwards point`.
//!    See `ligate-chain#245`.)
//! 5. Submit via `Submitter::submit_raw_tx`.
//!
//! Everything except step 1's `RuntimeCall` construction is generic
//! to any Sovereign-SDK chain. The Ligate-specific piece is just
//! "wrap a `bank::CallMessage` in `RuntimeCall::Bank`" using the
//! re-exported runtime call enum from `ligate-stf`.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use ligate_client::submit::Submitter;
use ligate_rollup::MockRollupSpec;
use ligate_stf::runtime::RuntimeCall;
use sov_bank::{Amount, CallMessage as BankCall, Coins, TokenId};
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::transaction::{PriorityFeeBips, UnsignedTransaction};
use sov_modules_api::{CryptoSpec, PrivateKey, PublicKey, Spec};
use thiserror::Error;

/// Concrete spec for transaction construction.
///
/// `MockRollupSpec<Native>` carries the same address shape
/// (`MultiAddressEvm`) and runtime composition as the production
/// chain. The DA flavour (Mock vs. Celestia) is a property of the
/// running node, not of the transaction; the chain hash that binds
/// the signature is identical across DA flavours per
/// `crates/stf/build.rs`. So the faucet can sign with this spec and
/// the chain accepts the tx whether it's actually running MockDA
/// (localnet) or Celestia (devnet).
type S = MockRollupSpec<Native>;
type ChainRuntime = ligate_stf::runtime::Runtime<S>;
type SovPrivateKey = <<S as Spec>::CryptoSpec as CryptoSpec>::PrivateKey;
type SovAddress = <S as Spec>::Address;

/// Default per-tx fee envelope (in nano-LGT). Generous so a faucet
/// drip never fails for fee reasons even if the chain's per-tx gas
/// burn drifts up. Operators can tune via env if needed.
const DEFAULT_MAX_FEE_NANO: u128 = 100_000_000; // 0.1 $LGT

#[derive(Debug, Error)]
pub enum SignerError {
    #[error("invalid recipient address: {0}")]
    InvalidAddress(String),
    #[error("invalid signer key: {0}")]
    InvalidSignerKey(String),
    #[error("chain submission failed: {0}")]
    SubmitFailed(String),
}

#[derive(Debug, Clone)]
pub struct DripReceipt {
    /// Transaction hash returned by the chain.
    pub tx_hash: String,
    /// Drip amount in nano-LGT.
    pub amount_nano: u128,
}

pub struct Signer {
    private_key: SovPrivateKey,
    submitter: Submitter,
    /// Chain RPC base URL with the `/v1` API prefix guaranteed.
    /// Used for HTTP polling on `/ledger/txs/{hash}` after submit.
    chain_rpc_with_v1: String,
    chain_hash: [u8; 32],
    chain_id: u64,
    lgt_token_id: TokenId,
    /// Local-counter nonce. Initialised from chain at startup, then
    /// monotonically incremented per drip. If the faucet restarts,
    /// re-fetch from chain (operator-side concern, not a signer
    /// invariant).
    nonce: AtomicU64,
}

// Manual Debug to keep the signing key out of any debug prints.
impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("private_key", &"<redacted>")
            .field("chain_id", &self.chain_id)
            .field("nonce", &self.nonce.load(Ordering::Relaxed))
            .finish()
    }
}

impl Signer {
    pub fn new(
        signing_key_hex: &str,
        chain_rpc: String,
        chain_id: u64,
        chain_hash: [u8; 32],
        lgt_token_id: TokenId,
        starting_nonce: u64,
    ) -> Result<Self, SignerError> {
        if signing_key_hex.len() != 64 {
            return Err(SignerError::InvalidSignerKey(format!(
                "expected 64 hex chars, got {}",
                signing_key_hex.len()
            )));
        }
        let key_bytes = hex::decode(signing_key_hex)
            .map_err(|e| SignerError::InvalidSignerKey(format!("hex decode: {e}")))?;
        let private_key = SovPrivateKey::try_from(key_bytes)
            .map_err(|e| SignerError::InvalidSignerKey(format!("key shape: {e:?}")))?;

        // Normalise the chain RPC URL to always end in `/v1`.
        // FAUCET_CHAIN_RPC accepts either `https://rpc.ligate.io` or
        // `https://rpc.ligate.io/v1`; the chain mounts its API under
        // `/v1/...` so the base passed to the SDK must include the
        // prefix.
        let trimmed = chain_rpc.trim_end_matches('/');
        let chain_rpc_with_v1 = if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        };

        Ok(Self {
            private_key,
            submitter: Submitter::new_unchecked(&chain_rpc_with_v1),
            chain_rpc_with_v1,
            chain_hash,
            chain_id,
            lgt_token_id,
            nonce: AtomicU64::new(starting_nonce),
        })
    }

    /// The faucet's own `lig1...` address derived from `private_key`.
    /// Used by the startup drip-budget check + for log lines so
    /// operators can `curl` the address's balance.
    pub fn address(&self) -> String {
        let pubkey = self.private_key.pub_key();
        let credential_id = pubkey.credential_id();
        SovAddress::from(credential_id).to_string()
    }

    /// Query the chain for the faucet's own LGT balance (nano-LGT).
    ///
    /// Used by the startup drip-budget sanity check
    /// (`FAUCET_MIN_DRIPS_BUDGET` in main.rs). Goes through the SDK's
    /// `get_balance_for_holder` so the URL shape stays in lockstep
    /// with what the rest of the chain client uses.
    ///
    /// Returns the balance as raw `u128` (nano-LGT), so the caller
    /// can divide by drip amount without an `Amount`-newtype roundtrip.
    pub async fn query_self_balance(&self) -> Result<u128, anyhow::Error> {
        use anyhow::Context;
        let addr = self.address();
        let amount = self
            .submitter
            .inner()
            .get_balance_for_holder::<S>(&addr, &self.lgt_token_id)
            .await
            .with_context(|| format!("querying LGT balance for {addr}"))?;
        // `Amount` is a newtype around u128; pull the inner.
        Ok(amount.0)
    }

    /// Sign and submit a `bank.transfer` of `amount_nano` from the
    /// signer's address to `recipient`. Returns the chain-issued tx
    /// hash once the chain has executed (success or failure).
    pub async fn drip(
        &self,
        recipient: &str,
        amount_nano: u128,
    ) -> Result<DripReceipt, SignerError> {
        // Parse the recipient lig1... bech32m address.
        let to: SovAddress = SovAddress::from_str(recipient)
            .map_err(|e| SignerError::InvalidAddress(format!("{recipient}: {e}")))?;

        // Build the runtime call. RuntimeCall<S> is the chain's
        // composed dispatch enum; we construct the bank-module
        // variant.
        let runtime_call: RuntimeCall<S> = RuntimeCall::Bank(BankCall::Transfer {
            to,
            coins: Coins {
                amount: Amount::from(amount_nano),
                token_id: self.lgt_token_id,
            },
        });

        // Reserve a nonce for this drip. Atomic so concurrent
        // requests get distinct nonces. If the chain rejects this
        // tx (e.g., insufficient balance), the nonce is "burned"
        // until the chain marks it used by a subsequent successful
        // tx.
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);

        // Wrap in unsigned tx envelope.
        let unsigned = UnsignedTransaction::<ChainRuntime, S>::new(
            runtime_call,
            self.chain_id,
            PriorityFeeBips::ZERO,
            Amount::from(DEFAULT_MAX_FEE_NANO),
            UniquenessData::Nonce(nonce),
            None, // gas_limit: None = chain-default
        );

        // Sign. Binds to chain_hash so the signature only verifies
        // on this chain id.
        let signed = unsigned.sign(&self.private_key, &self.chain_hash);

        // Borsh-encode the signed `Transaction`. The chain's
        // `POST /v1/sequencer/txs` handler accepts the inner signed tx
        // bytes directly and wraps them in `AuthenticatorInput::Standard`
        // server-side (see `sov-sequencer::rest_api::axum_accept_tx`).
        // Pre-wrapping here would double-wrap and the chain would
        // reject with "Cannot decompress Edwards point" (chain #245).
        let signed_bytes = borsh::to_vec(&signed)
            .map_err(|e| SignerError::SubmitFailed(format!("encoding signed tx: {e}")))?;

        // Submit. `wait_for_inclusion = false` because the SDK's
        // `wait_for_tx_processing` uses a WebSocket subscription that
        // hits a URL-parse bug in our setup (`invalid port value`,
        // see ligate-cli#8). We do an HTTP poll on
        // `/ledger/txs/{hash}` below instead.
        let tx_hash = self
            .submitter
            .submit_raw_tx(signed_bytes, /* wait */ false)
            .await
            .map_err(|e| SignerError::SubmitFailed(format!("submit: {e:#}")))?;
        let tx_hash_str = tx_hash.to_string();

        // Poll for inclusion. Returns once the chain has indexed the
        // tx (success or failure both count) or times out. The drip
        // request is held open until inclusion so the user gets a
        // useful response shape (`tx_hash` they can verify against
        // the explorer immediately, not eventually).
        self.wait_for_inclusion(&tx_hash_str).await?;

        Ok(DripReceipt {
            tx_hash: tx_hash_str,
            amount_nano,
        })
    }

    /// Poll the chain via `GET /ledger/txs/{tx_hash}` until the
    /// transaction has been indexed. See ligate-cli#8 for context;
    /// equivalent to the cli's helper of the same name.
    async fn wait_for_inclusion(&self, tx_hash: &str) -> Result<(), SignerError> {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

        let url = format!("{}/ledger/txs/{tx_hash}", self.chain_rpc_with_v1);
        let started = std::time::Instant::now();
        loop {
            if started.elapsed() > MAX_WAIT {
                return Err(SignerError::SubmitFailed(format!(
                    "timed out after {:?} waiting for tx {tx_hash} to be included; \
                     drip may still land — check {url} to verify",
                    MAX_WAIT
                )));
            }
            if self.submitter.inner().http_get(&url).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zeroed-out token id for unit tests. The signer doesn't actually
    /// touch the token id during construction, so the value is
    /// arbitrary; we just need _some_ `TokenId` to plug in.
    fn zero_token_id() -> TokenId {
        TokenId::from([0u8; 32])
    }

    #[test]
    fn rejects_too_short_key() {
        let err = Signer::new(
            "abcd",
            "http://localhost:12346".into(),
            1,
            [0u8; 32],
            zero_token_id(),
            0,
        )
        .unwrap_err();
        assert!(matches!(err, SignerError::InvalidSignerKey(_)));
    }

    #[test]
    fn rejects_non_hex_key() {
        let err = Signer::new(
            &"z".repeat(64),
            "http://localhost:12346".into(),
            1,
            [0u8; 32],
            zero_token_id(),
            0,
        )
        .unwrap_err();
        assert!(matches!(err, SignerError::InvalidSignerKey(_)));
    }

    #[test]
    fn accepts_valid_key() {
        let key = "00".repeat(32);
        let _ = Signer::new(
            &key,
            "http://localhost:12346".into(),
            1,
            [0u8; 32],
            zero_token_id(),
            0,
        )
        .unwrap();
    }
}
