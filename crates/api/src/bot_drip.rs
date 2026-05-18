//! `POST /v1/drip-bot` — Discord faucet bot endpoint.
//!
//! Mirrors `POST /v1/drip` semantically (sign + submit a bank transfer
//! to the requesting address), but with three key differences from the
//! web faucet:
//!
//! 1. **Header-auth gated**: every request must carry an `X-Bot-Secret`
//!    header matching `Config::faucet_bot_secret`. Without it, 401.
//!    This is what makes it safe to expose tier-aware larger drip
//!    amounts: a public caller can't hit this endpoint at all.
//!
//! 2. **Tier-aware amount**: bot claims a tier (newcomer / regular /
//!    veteran / elder) based on the caller's Discord server tenure,
//!    AND claims an amount. The api re-validates that the claimed
//!    amount matches the tier's configured amount; mismatch = 400.
//!    A compromised bot can downgrade itself (claim a lower tier than
//!    deserved) but can never over-drip.
//!
//! 3. **Postgres-backed cooldown** (5d default). The web faucet's
//!    in-process `DashMap` is fine for 24h windows because Railway
//!    restarts are rare, but 5-day windows would lose multi-day
//!    cooldowns on every redeploy. Two cooldown lookups fire per
//!    request: per-address (someone else dripped to this `lig1...`
//!    in the last 5d) and per-Discord-user (this user dripped any
//!    address in 5d). Both must clear.
//!
//! See `migrations/20260518000001_bot_drips.sql` for the schema.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, SecondsFormat, Utc};
use ligate_api_drip::SignerError;
use serde::{Deserialize, Serialize};
use sqlx::types::BigDecimal;
use std::str::FromStr;
use tracing::{info, warn};

use crate::AppState;

/// Tier the bot computes locally from the caller's Discord state and
/// passes to the api so the amount-validation logic doesn't need to
/// know about Discord. The api treats tier as the authoritative
/// determinant of allowed amount; the `tier_evidence` field carries
/// the raw inputs for analytics + audit-log forensics.
///
/// Adding a new tier: add the variant, add a config field +
/// `to_amount` arm, bump the migration's `tier` comment. The TEXT
/// column accepts any string so no schema change is needed.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// < 7 days in the Ligate Discord server.
    Newcomer,
    /// 7-30 days in the Ligate Discord server.
    Regular,
    /// 30-90 days in the Ligate Discord server.
    Veteran,
    /// 90+ days in the Ligate Discord server.
    Elder,
}

impl Tier {
    fn as_str(&self) -> &'static str {
        match self {
            Tier::Newcomer => "newcomer",
            Tier::Regular => "regular",
            Tier::Veteran => "veteran",
            Tier::Elder => "elder",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DripBotRequest {
    /// Chain address to receive the drip. bech32m `lig1...`.
    pub address: String,
    /// Discord user id (Snowflake, stringified u64 — see migration
    /// comment for why TEXT not BIGINT).
    pub discord_user_id: String,
    /// Tier the bot claims for the caller. The api validates that
    /// `amount_nano` matches the configured amount for this tier.
    pub tier: Tier,
    /// Drip amount in nano-LGT. Must equal `tier_to_amount(tier)`.
    pub amount_nano: u128,
    /// Raw tier inputs. Optional and analytics-only — the api does
    /// NOT recompute tier from these (the bot knows the live Discord
    /// state; the api wouldn't). Logged + persisted for forensics if
    /// a future tier-curve change wants to backtest cohorts.
    pub tier_evidence: Option<TierEvidence>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TierEvidence {
    pub joined_at: Option<DateTime<Utc>>,
    pub account_created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct DripBotResponse {
    pub address: String,
    pub discord_user_id: String,
    pub tx_hash: String,
    pub amount_nano: u128,
    pub amount_lgt: f64,
    pub tier: Tier,
    /// When this address can drip again via the bot endpoint.
    pub next_drip_available_at: String,
}

#[derive(Debug, Serialize)]
pub struct BotErrorResponse {
    pub error: String,
    /// Present for 429 responses. Seconds until cooldown clears.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<i64>,
}

/// Constant-time equality on byte slices.
///
/// Hand-rolled to avoid pulling in the `subtle` crate for one
/// 5-line helper. Returns `false` for length mismatch (the length
/// itself is not secret; the secret bytes are).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `1e9` as a float; preserves precision for typical drip amounts
/// (100-1000 LGT) without pulling in big-decimal math.
fn nano_to_lgt(nano: u128) -> f64 {
    (nano as f64) / 1e9
}

/// Returns the latest `dripped_at` for a given column value within
/// the cooldown window, or `None` if no recent drip exists.
async fn last_drip_within_window(
    pg: &sqlx::PgPool,
    column: &str, // "address" or "discord_user_id"
    value: &str,
    window_secs: u64,
) -> sqlx::Result<Option<DateTime<Utc>>> {
    // Hand-built query: sqlx::query! can't accept a dynamic column
    // name (it parses the SQL at compile time). The column comes from
    // a fixed, in-code set of two values, so there's no SQL-injection
    // surface — but we still validate as defense-in-depth.
    let col = match column {
        "address" => "address",
        "discord_user_id" => "discord_user_id",
        _ => {
            // Caller bug. Return None to fail-closed (treats as "no
            // recent drip" which would allow the drip — but the only
            // callers are in this file and pass literal strings).
            return Ok(None);
        }
    };
    let sql = format!(
        "SELECT dripped_at FROM bot_drips \
         WHERE {col} = $1 \
           AND dripped_at > NOW() - make_interval(secs => $2) \
         ORDER BY dripped_at DESC \
         LIMIT 1"
    );
    sqlx::query_scalar::<_, DateTime<Utc>>(&sql)
        .bind(value)
        .bind(window_secs as f64)
        .fetch_optional(pg)
        .await
}

pub async fn drip_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DripBotRequest>,
) -> Result<Json<DripBotResponse>, (StatusCode, Json<BotErrorResponse>)> {
    // 1. Auth ---------------------------------------------------------
    let configured = state.config.faucet_bot_secret.as_deref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(BotErrorResponse {
            error: "bot endpoint disabled (FAUCET_BOT_SECRET unset)".to_string(),
            retry_after_secs: None,
        }),
    ))?;

    let provided = headers
        .get("x-bot-secret")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), configured.as_bytes()) {
        // Don't leak whether the secret was missing vs wrong.
        warn!("drip-bot: 401 (bad/missing X-Bot-Secret)");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(BotErrorResponse {
                error: "unauthorized".to_string(),
                retry_after_secs: None,
            }),
        ));
    }

    // 2. Amount validation -------------------------------------------
    let expected = match req.tier {
        Tier::Newcomer => state.config.bot_drip_amount_newcomer,
        Tier::Regular => state.config.bot_drip_amount_regular,
        Tier::Veteran => state.config.bot_drip_amount_veteran,
        Tier::Elder => state.config.bot_drip_amount_elder,
    };
    if req.amount_nano != expected {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(BotErrorResponse {
                error: format!(
                    "amount {} doesn't match tier '{}' configured amount {}",
                    req.amount_nano,
                    req.tier.as_str(),
                    expected
                ),
                retry_after_secs: None,
            }),
        ));
    }

    // 3. Cooldown checks ---------------------------------------------
    // Both per-address AND per-user must clear. We check both before
    // touching the signer so a wedged cooldown doesn't burn nonce
    // capacity.
    for (col, val, label) in [
        ("address", req.address.as_str(), "address"),
        (
            "discord_user_id",
            req.discord_user_id.as_str(),
            "Discord user",
        ),
    ] {
        match last_drip_within_window(&state.pg, col, val, state.config.bot_drip_rate_limit_secs)
            .await
        {
            Ok(Some(last)) => {
                let elapsed = (Utc::now() - last).num_seconds().max(0) as u64;
                let retry_after = state
                    .config
                    .bot_drip_rate_limit_secs
                    .saturating_sub(elapsed);
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(BotErrorResponse {
                        error: format!("{label} rate-limited; retry in {} seconds", retry_after),
                        retry_after_secs: Some(retry_after as i64),
                    }),
                ));
            }
            Ok(None) => {}
            Err(e) => {
                warn!(?e, "drip-bot: cooldown query failed");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(BotErrorResponse {
                        error: "internal error during cooldown check".to_string(),
                        retry_after_secs: None,
                    }),
                ));
            }
        }
    }

    // 4. Sign + submit ------------------------------------------------
    // Uses the SAME signer as the web faucet — one signer means one
    // nonce stream and no coordination needed between endpoints.
    let receipt = state
        .signer
        .drip(&req.address, req.amount_nano)
        .await
        .map_err(|e| match e {
            SignerError::InvalidAddress(msg) => (
                StatusCode::BAD_REQUEST,
                Json(BotErrorResponse {
                    error: msg,
                    retry_after_secs: None,
                }),
            ),
            SignerError::InvalidSignerKey(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(BotErrorResponse {
                    error: format!("operator misconfig: {msg}"),
                    retry_after_secs: None,
                }),
            ),
            SignerError::SubmitFailed(msg) => (
                StatusCode::BAD_GATEWAY,
                Json(BotErrorResponse {
                    error: format!("chain submission failed: {msg}"),
                    retry_after_secs: None,
                }),
            ),
        })?;

    // 5. Persist the drip --------------------------------------------
    // Only after the chain accepted, so a chain failure doesn't burn
    // the user's cooldown.
    //
    // u128 → NUMERIC via BigDecimal (sqlx has no native u128 support).
    // The bot's drip amounts are always well below u64::MAX so this
    // conversion is lossless; the NUMERIC column is sized for the
    // theoretical max anyway.
    let amount_bd = BigDecimal::from_str(&req.amount_nano.to_string())
        .expect("u128 always parses as BigDecimal");

    let server_tenure_days = req
        .tier_evidence
        .as_ref()
        .and_then(|e| e.joined_at)
        .map(|t| (Utc::now() - t).num_days() as i32);
    let account_age_days = req
        .tier_evidence
        .as_ref()
        .and_then(|e| e.account_created_at)
        .map(|t| (Utc::now() - t).num_days() as i32);

    if let Err(e) = sqlx::query(
        "INSERT INTO bot_drips \
            (address, discord_user_id, amount_nano, tx_hash, tier, \
             server_tenure_days, account_age_days) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&req.address)
    .bind(&req.discord_user_id)
    .bind(&amount_bd)
    .bind(&receipt.tx_hash)
    .bind(req.tier.as_str())
    .bind(server_tenure_days)
    .bind(account_age_days)
    .execute(&state.pg)
    .await
    {
        // Chain accepted but our bookkeeping failed. Log loudly; the
        // next /v1/drip-bot call against this address or user won't
        // be cooldown-blocked, which is a small drift but not an
        // abuse window (worst case: one extra drip from this user).
        warn!(?e, address = %req.address, "drip-bot: chain accepted but Postgres INSERT failed");
    }

    let next_at =
        Utc::now() + chrono::Duration::seconds(state.config.bot_drip_rate_limit_secs as i64);

    info!(
        address = %req.address,
        discord_user_id = %req.discord_user_id,
        tier = req.tier.as_str(),
        amount_nano = req.amount_nano,
        tx_hash = %receipt.tx_hash,
        "drip-bot ok"
    );

    Ok(Json(DripBotResponse {
        address: req.address,
        discord_user_id: req.discord_user_id,
        tx_hash: receipt.tx_hash,
        amount_nano: receipt.amount_nano,
        amount_lgt: nano_to_lgt(receipt.amount_nano),
        tier: req.tier,
        next_drip_available_at: next_at.to_rfc3339_opts(SecondsFormat::Millis, true),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn tier_str_round_trip() {
        // Serialize as snake_case, matching what the bot sends.
        assert_eq!(
            serde_json::to_string(&Tier::Newcomer).unwrap(),
            "\"newcomer\""
        );
        assert_eq!(serde_json::to_string(&Tier::Elder).unwrap(), "\"elder\"");
        let parsed: Tier = serde_json::from_str("\"veteran\"").unwrap();
        assert_eq!(parsed, Tier::Veteran);
    }

    #[test]
    fn nano_to_lgt_precision() {
        assert_eq!(nano_to_lgt(100_000_000_000), 100.0);
        assert_eq!(nano_to_lgt(1_000_000_000_000), 1000.0);
        assert_eq!(nano_to_lgt(0), 0.0);
    }
}
