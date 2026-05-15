//! Env-var-driven config. Read once at startup, never reloaded.

use anyhow::{anyhow, Context, Result};

const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_DRIP_AMOUNT: u128 = 100_000_000_000; // 100 LGT in nano-LGT
const DEFAULT_DRIP_RATE_LIMIT_SECS: u64 = 24 * 60 * 60; // 24h per address
const DEFAULT_DRIP_MIN_BUDGET: u64 = 100;
const DEFAULT_PG_POOL_SIZE: u32 = 10;

/// All env-derived runtime config for `ligate-api`.
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTP server bind address. Default `0.0.0.0:8080`.
    pub bind: String,

    /// Postgres connection URL (Railway-managed in production, local
    /// `postgres://...` for dev).
    pub database_url: String,

    /// Postgres pool size. Default 10. Bump on a busy public node.
    pub pg_pool_size: u32,

    /// URL of the Ligate Chain REST API. Default
    /// `http://127.0.0.1:12346` for localnet; production points at
    /// `https://rpc.ligate.io`.
    pub chain_rpc: String,

    /// Numeric chain id (u64, NOT the human `ligate-devnet-1` string).
    /// From the chain's `chain_state.json`.
    pub chain_id: u64,

    /// 32-byte chain hash from `GET /v1/rollup/info`. Captured at boot
    /// for predictability; restart the api after a chain re-genesis.
    pub chain_hash: [u8; 32],

    /// LGT token id, 64-char hex from `bank.json`'s `gas_token_config`.
    pub lgt_token_id_hex: String,

    /// Drip signer hot-key, 64-char hex (32-byte ed25519 private key).
    /// Held in process memory; never logged.
    pub drip_signer_key: String,

    /// Per-drip amount in nano-LGT. Default `100_000_000_000` (100 LGT).
    pub drip_amount: u128,

    /// Per-address rate-limit cooldown. Default 24h.
    pub drip_rate_limit_secs: u64,

    /// Startup balance check: refuse to start if signer's balance covers
    /// fewer than this many drips. Default 100. `0` to disable.
    pub drip_min_budget: u64,

    /// Starting nonce override for the drip signer.
    ///
    /// - `None` (DRIP_STARTING_NONCE unset): the api queries the chain
    ///   on startup and uses the current on-chain nonce. The right
    ///   default; survives Railway redeploys without operator action.
    /// - `Some(n)` (DRIP_STARTING_NONCE=n): use `n` verbatim, skip the
    ///   chain query. Escape hatch for offline boots, recovery from a
    ///   wedged uniqueness state, or chain-RPC outages at startup.
    pub drip_starting_nonce: Option<u64>,

    /// Slot height to start the indexer ingest from. `None` means
    /// resume from DB or 1 if empty.
    pub indexer_start_height: Option<u64>,

    /// Treasury address (bech32m `lig1...`) used by the
    /// `/v1/stats/totals` endpoint to surface treasury balance as
    /// part of the "key numbers" view.
    ///
    /// Optional: when unset, the totals endpoint omits the
    /// `treasury_balance_nano` + `treasury_address` fields and
    /// returns the rest of the response intact. Genesis pins the
    /// real treasury at `chain/devnet-1/genesis/bank.json`; partners
    /// running their own chain copy can leave this unset.
    pub lgt_treasury_addr: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // `API_BIND` wins if set explicitly. Otherwise honour `PORT`
        // (Railway / Heroku convention) by binding to `0.0.0.0:$PORT`.
        // Falls back to the default if neither is set.
        let bind = std::env::var("API_BIND")
            .ok()
            .or_else(|| std::env::var("PORT").ok().map(|p| format!("0.0.0.0:{p}")))
            .unwrap_or_else(|| DEFAULT_BIND.to_string());

        let database_url = std::env::var("DATABASE_URL")
            .context("DATABASE_URL is required (Postgres connection string)")?;

        let pg_pool_size = parse_env_u32("PG_POOL_SIZE", DEFAULT_PG_POOL_SIZE)?;

        let chain_rpc =
            std::env::var("CHAIN_RPC").unwrap_or_else(|_| "http://127.0.0.1:12346".to_string());

        let chain_id = std::env::var("CHAIN_ID")
            .context("CHAIN_ID is required (numeric, from chain_state.json)")?
            .parse::<u64>()
            .context("CHAIN_ID must be u64")?;

        let chain_hash_hex =
            std::env::var("CHAIN_HASH").context("CHAIN_HASH is required (64-char hex)")?;
        if chain_hash_hex.len() != 64 {
            return Err(anyhow!(
                "CHAIN_HASH must be 64 hex chars, got {}",
                chain_hash_hex.len()
            ));
        }
        let chain_hash_bytes =
            hex::decode(&chain_hash_hex).context("CHAIN_HASH must be valid hex")?;
        let mut chain_hash = [0u8; 32];
        chain_hash.copy_from_slice(&chain_hash_bytes);

        let lgt_token_id_hex = std::env::var("LGT_TOKEN_ID")
            .context("LGT_TOKEN_ID is required (64-char hex from bank.json)")?;

        let drip_signer_key = std::env::var("DRIP_SIGNER_KEY")
            .context("DRIP_SIGNER_KEY is required (64-char hex private key)")?;
        if drip_signer_key.len() != 64 {
            return Err(anyhow!(
                "DRIP_SIGNER_KEY must be 64 hex chars (32 bytes), got {}",
                drip_signer_key.len()
            ));
        }
        if hex::decode(&drip_signer_key).is_err() {
            return Err(anyhow!("DRIP_SIGNER_KEY must be valid hex"));
        }

        let drip_amount = parse_env_u128("DRIP_AMOUNT", DEFAULT_DRIP_AMOUNT)?;
        let drip_rate_limit_secs =
            parse_env_u64("DRIP_RATE_LIMIT_SECS", DEFAULT_DRIP_RATE_LIMIT_SECS)?;
        let drip_min_budget = parse_env_u64("DRIP_MIN_BUDGET", DEFAULT_DRIP_MIN_BUDGET)?;
        let drip_starting_nonce = std::env::var("DRIP_STARTING_NONCE")
            .ok()
            .map(|s| s.parse::<u64>())
            .transpose()
            .context("DRIP_STARTING_NONCE must be u64")?;

        let indexer_start_height = std::env::var("INDEXER_START_HEIGHT")
            .ok()
            .map(|s| s.parse::<u64>())
            .transpose()
            .context("INDEXER_START_HEIGHT must be u64")?;

        let lgt_treasury_addr = std::env::var("LGT_TREASURY_ADDR").ok();

        Ok(Self {
            bind,
            database_url,
            pg_pool_size,
            chain_rpc,
            chain_id,
            chain_hash,
            lgt_token_id_hex,
            drip_signer_key,
            drip_amount,
            drip_rate_limit_secs,
            drip_min_budget,
            drip_starting_nonce,
            indexer_start_height,
            lgt_treasury_addr,
        })
    }
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32> {
    std::env::var(name)
        .ok()
        .map(|s| s.parse::<u32>())
        .transpose()
        .with_context(|| format!("{name} must be u32"))
        .map(|v| v.unwrap_or(default))
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    std::env::var(name)
        .ok()
        .map(|s| s.parse::<u64>())
        .transpose()
        .with_context(|| format!("{name} must be u64"))
        .map(|v| v.unwrap_or(default))
}

fn parse_env_u128(name: &str, default: u128) -> Result<u128> {
    std::env::var(name)
        .ok()
        .map(|s| s.parse::<u128>())
        .transpose()
        .with_context(|| format!("{name} must be u128"))
        .map(|v| v.unwrap_or(default))
}
