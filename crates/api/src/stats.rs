//! `/v1/stats/*` aggregate analytics endpoints.
//!
//! Powers the explorer's "key numbers" row, the investor dashboard
//! panels in Grafana, and any third-party indexer that wants
//! pre-aggregated views without re-implementing the queries.
//!
//! ## Endpoints (mounted under `/v1/stats/`)
//!
//! | Path | What it returns |
//! |------|-----------------|
//! | `GET /totals` | Single object with all chain-level counts: blocks, txs (total + success), addresses, schemas, attestor sets, attestations, last indexed slot. |
//! | `GET /active-addresses?window=24h` | Unique addresses with at least one tx (sent or received) in the time window. |
//! | `GET /new-wallets-daily?days=7` | Timeseries of new addresses (`address_summaries.first_seen_timestamp`) bucketed by UTC day. |
//! | `GET /tx-rate-daily?days=7` | Timeseries of tx counts bucketed by UTC day, broken down by `kind` (bank.transfer, register.schema, etc.) and `outcome`. |
//! | `GET /top-holders?n=10` | Top N LGT holders by current balance, queried live from the chain's bank module (no balance index in the api yet; fine for devnet's ~10-address scale, replace with indexed column before mainnet). |
//!
//! ## Caching
//!
//! All responses are served from a 30s in-process [`StatsCache`]. The
//! cache TTL matches the typical Grafana scrape cadence, so concurrent
//! dashboard sessions hit Postgres + chain RPC at most once per 30s
//! per endpoint. Responses also carry `Cache-Control: public,
//! max-age=30` so any reverse proxy or browser respects the same
//! window.
//!
//! ## Auth
//!
//! Public read-only. The data is reconstructable from the existing
//! per-entity routes (`/v1/blocks`, `/v1/txs`, `/v1/addresses/{addr}`),
//! these endpoints just save the caller a few hundred round-trips.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::AppState;

/// Default TTL for cached stats responses. Tuned to match the typical
/// Grafana scrape cadence (30s) so concurrent dashboard sessions
/// don't multiply Postgres / chain-RPC load.
const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// In-process cache of serialized stats responses, keyed by a stable
/// per-endpoint string (including query-param fingerprint). Stores
/// the serialized JSON body verbatim so a cache hit is a `String`
/// clone, not a re-render of the typed response.
///
/// `DashMap` over `Mutex<HashMap>` for the read-heavy access pattern
/// (every request reads; only the request that missed writes).
#[derive(Clone, Default)]
pub struct StatsCache {
    inner: Arc<DashMap<String, (Instant, String)>>,
}

impl StatsCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get_fresh(&self, key: &str, ttl: Duration) -> Option<String> {
        let entry = self.inner.get(key)?;
        if entry.0.elapsed() < ttl {
            Some(entry.1.clone())
        } else {
            None
        }
    }

    fn put(&self, key: String, value: String) {
        self.inner.insert(key, (Instant::now(), value));
    }
}

/// Wrap a serialized JSON body in a `Cache-Control`-tagged 200
/// response. Centralised so adding headers (eg. `Vary`, `ETag`)
/// later only touches one site.
fn cached_json_response(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=30"),
        ],
        body,
    )
        .into_response()
}

/// Convert an `anyhow::Error` into a 500 JSON body. Stats endpoints
/// are best-effort (the indexer or chain RPC may transiently fail);
/// surface the error to the caller verbatim rather than swallowing.
fn error_response(err: anyhow::Error) -> Response {
    tracing::warn!(error = %err, "stats endpoint failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"error":"{}"}}"#, err.to_string().replace('"', "'")),
    )
        .into_response()
}

// ---- /totals ---------------------------------------------------------------

#[derive(Serialize)]
struct TotalsResponse {
    /// Highest slot the indexer has committed to Postgres. Hint for
    /// callers that want to detect indexer lag without a separate
    /// `/v1/info` round-trip.
    indexed_at_slot: i64,
    /// Total blocks (slots) indexed. Equal to chain block height
    /// once the indexer catches up.
    blocks: i64,
    /// Every tx the indexer has recorded, including reverted ones.
    txs_total: i64,
    /// Subset of `txs_total` that the chain committed. The indexer
    /// writes `outcome = 'committed'` for chain `result = "successful"`
    /// per the RFC 0002 mapping (`crates/indexer/src/parser.rs`).
    /// Field name matches the value the indexer stores, not the
    /// chain-side `"successful"` label.
    txs_committed: i64,
    /// Distinct addresses seen as a tx sender or recipient.
    addresses: i64,
    /// Registered attestation schemas (`RegisterSchema` txs).
    schemas: i64,
    /// Registered attestor sets (`RegisterAttestorSet` txs).
    attestor_sets: i64,
    /// Submitted attestations (`SubmitAttestation` txs).
    attestations: i64,
    /// Total LGT supply in nano-LGT (u128 as decimal string). Pulled
    /// live from the chain's bank module at compute time; cached in
    /// the response for `DEFAULT_TTL`. `None` when chain RPC is
    /// unreachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_supply_nano: Option<String>,
    /// Treasury balance in nano-LGT (u128 as decimal string). `None`
    /// when either chain RPC is unreachable OR `LGT_TREASURY_ADDR`
    /// is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    treasury_balance_nano: Option<String>,
    /// Treasury address (bech32m `lig1...`) the balance above refers
    /// to. Surfaced so clients can deep-link to the address page;
    /// `None` iff `treasury_balance_nano` is also `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    treasury_address: Option<String>,
}

pub async fn totals(State(state): State<AppState>) -> Response {
    let key = "totals";
    if let Some(cached) = state.stats_cache.get_fresh(key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    match compute_totals(&state).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key.to_string(), body.clone());
                cached_json_response(body)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_totals(state: &AppState) -> anyhow::Result<TotalsResponse> {
    let pool = &state.pg;
    // Eight scalar queries in series. Each is an O(1) `COUNT(*)` on a
    // small table plus a `MAX(height)`. Total wall-clock is bounded
    // by Postgres round-trip * 8 (~10-30ms on a hot connection),
    // dwarfed by the 30s cache TTL.
    let blocks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM slots")
        .fetch_one(pool)
        .await
        .context("count slots")?;
    let txs_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transactions")
        .fetch_one(pool)
        .await
        .context("count txs total")?;
    let txs_committed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transactions WHERE outcome = 'committed'")
            .fetch_one(pool)
            .await
            .context("count txs committed")?;
    let addresses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM address_summaries")
        .fetch_one(pool)
        .await
        .context("count addresses")?;
    let schemas: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schemas")
        .fetch_one(pool)
        .await
        .context("count schemas")?;
    let attestor_sets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attestor_sets")
        .fetch_one(pool)
        .await
        .context("count attestor_sets")?;
    let attestations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attestations")
        .fetch_one(pool)
        .await
        .context("count attestations")?;
    let indexed_at_slot: Option<i64> = sqlx::query_scalar("SELECT MAX(height) FROM slots")
        .fetch_one(pool)
        .await
        .context("max slot")?;

    // Chain-side reads. Both are best-effort: a chain RPC blip
    // shouldn't kill the whole stats endpoint, so failures degrade
    // to `None` rather than propagating up. Operators see the
    // tracing::warn line and the response continues to serve the
    // indexer-derived counts.
    let total_supply_nano = match state.signer.query_total_supply().await {
        Ok(n) => Some(n.to_string()),
        Err(e) => {
            tracing::warn!(error = %e, "total-supply query failed; omitting from /v1/stats/totals");
            None
        }
    };
    let (treasury_balance_nano, treasury_address) = match &state.config.lgt_treasury_addr {
        Some(addr) => match state.signer.query_balance_for(addr).await {
            Ok(n) => (Some(n.to_string()), Some(addr.clone())),
            Err(e) => {
                tracing::warn!(
                    address = %addr,
                    error = %e,
                    "treasury balance query failed; omitting from /v1/stats/totals"
                );
                (None, None)
            }
        },
        None => (None, None),
    };

    Ok(TotalsResponse {
        indexed_at_slot: indexed_at_slot.unwrap_or(0),
        blocks,
        txs_total,
        txs_committed,
        addresses,
        schemas,
        attestor_sets,
        attestations,
        total_supply_nano,
        treasury_balance_nano,
        treasury_address,
    })
}

// ---- /active-addresses -----------------------------------------------------

#[derive(Deserialize)]
pub struct ActiveAddressesQuery {
    /// Window like `24h`, `7d`, `1h`. Default `24h`.
    #[serde(default)]
    window: Option<String>,
}

#[derive(Serialize)]
struct ActiveAddressesResponse {
    window: String,
    since: String,
    count: i64,
}

pub async fn active_addresses(
    State(state): State<AppState>,
    Query(params): Query<ActiveAddressesQuery>,
) -> Response {
    let window = params.window.unwrap_or_else(|| "24h".to_string());
    let interval = match parse_interval(&window) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    let key = format!("active-addresses:{window}");
    if let Some(cached) = state.stats_cache.get_fresh(&key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    match compute_active_addresses(&state.pg, &window, interval).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_active_addresses(
    pool: &PgPool,
    window_label: &str,
    interval: Duration,
) -> anyhow::Result<ActiveAddressesResponse> {
    let since = Utc::now() - chrono::Duration::from_std(interval).context("window too long")?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM address_summaries WHERE last_seen_timestamp >= $1",
    )
    .bind(since)
    .fetch_one(pool)
    .await
    .context("count active addresses")?;
    Ok(ActiveAddressesResponse {
        window: window_label.to_string(),
        since: since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        count,
    })
}

// ---- /new-wallets-daily ----------------------------------------------------

#[derive(Deserialize)]
pub struct NewWalletsDailyQuery {
    /// How many days of history to return. Default 7, capped at 90.
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Serialize)]
struct DailyPoint {
    /// UTC date as `YYYY-MM-DD`.
    date: String,
    /// Addresses whose `first_seen_timestamp` fell on this UTC day.
    count: i64,
}

#[derive(Serialize)]
struct NewWalletsDailyResponse {
    days: u32,
    points: Vec<DailyPoint>,
}

pub async fn new_wallets_daily(
    State(state): State<AppState>,
    Query(params): Query<NewWalletsDailyQuery>,
) -> Response {
    let days = params.days.unwrap_or(7).min(90);
    let key = format!("new-wallets-daily:{days}");
    if let Some(cached) = state.stats_cache.get_fresh(&key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    match compute_new_wallets_daily(&state.pg, days).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_new_wallets_daily(
    pool: &PgPool,
    days: u32,
) -> anyhow::Result<NewWalletsDailyResponse> {
    let since = Utc::now() - chrono::Duration::days(days as i64);
    // `DATE_TRUNC('day', first_seen_timestamp)` buckets by UTC day.
    let rows: Vec<(DateTime<Utc>, i64)> = sqlx::query_as(
        "SELECT DATE_TRUNC('day', first_seen_timestamp) AS day, COUNT(*) AS new_wallets \
         FROM address_summaries \
         WHERE first_seen_timestamp >= $1 \
         GROUP BY day \
         ORDER BY day ASC",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .context("daily new-wallets bucket")?;
    let points = rows
        .into_iter()
        .map(|(day, count)| DailyPoint {
            date: day.format("%Y-%m-%d").to_string(),
            count,
        })
        .collect();
    Ok(NewWalletsDailyResponse { days, points })
}

// ---- /tx-rate-daily --------------------------------------------------------

#[derive(Deserialize)]
pub struct TxRateDailyQuery {
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Serialize)]
struct TxRatePoint {
    date: String,
    kind: String,
    outcome: String,
    count: i64,
}

#[derive(Serialize)]
struct TxRateDailyResponse {
    days: u32,
    points: Vec<TxRatePoint>,
}

pub async fn tx_rate_daily(
    State(state): State<AppState>,
    Query(params): Query<TxRateDailyQuery>,
) -> Response {
    let days = params.days.unwrap_or(7).min(90);
    let key = format!("tx-rate-daily:{days}");
    if let Some(cached) = state.stats_cache.get_fresh(&key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    match compute_tx_rate_daily(&state.pg, days).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_tx_rate_daily(pool: &PgPool, days: u32) -> anyhow::Result<TxRateDailyResponse> {
    let since = Utc::now() - chrono::Duration::days(days as i64);
    // Group by (day, kind, outcome). Stacked-area chart in Grafana
    // can pivot however the viewer wants.
    let rows: Vec<(DateTime<Utc>, String, String, i64)> = sqlx::query_as(
        "SELECT \
            DATE_TRUNC('day', s.timestamp) AS day, \
            t.kind, \
            t.outcome, \
            COUNT(*) AS count \
         FROM transactions t \
         JOIN slots s ON s.height = t.slot \
         WHERE s.timestamp >= $1 \
         GROUP BY day, t.kind, t.outcome \
         ORDER BY day ASC, t.kind ASC, t.outcome ASC",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .context("daily tx-rate bucket")?;
    let points = rows
        .into_iter()
        .map(|(day, kind, outcome, count)| TxRatePoint {
            date: day.format("%Y-%m-%d").to_string(),
            kind,
            outcome,
            count,
        })
        .collect();
    Ok(TxRateDailyResponse { days, points })
}

// ---- /finality -------------------------------------------------------------

/// DA finalization-latency stats for the explorer + investor
/// dashboards.
///
/// **v0 returns estimated values, not observations.** The api doesn't
/// yet subscribe to the chain's `BlobExecutionStatus` broadcast
/// channel; the chain's `ligate_da_finalization_latency_seconds`
/// Prometheus histogram has the real data but isn't reachable from
/// the api's network. So this endpoint hardcodes Mocha-derived
/// estimates (~12s p50, ~15s p95/p99) so the explorer has a real
/// field to render and the response *shape* is forward-compatible
/// for when we wire actual sampling.
///
/// When sampling lands (tracked in `ligate-api#TBD`), the wire shape
/// stays identical; only the `source` field flips from `"estimated"`
/// to `"observed"`. Clients can branch on `source` if they want to
/// surface that distinction in the UI ("typical" vs "live").
#[derive(Serialize)]
struct FinalityResponse {
    /// Window the percentiles are computed over. `"static"` for the
    /// estimated v0; `"1h"` / `"24h"` etc. when sampling is wired.
    window: String,
    /// Number of finalization observations in the window. `0` while
    /// `source == "estimated"`.
    sampled_count: u32,
    /// Median DA finalization latency in seconds.
    p50_seconds: f64,
    /// 95th percentile.
    p95_seconds: f64,
    /// 99th percentile.
    p99_seconds: f64,
    /// DA layer the rollup is anchored to. Constant per chain.
    da_layer: String,
    /// `"estimated"` when values come from a static config-derived
    /// model; `"observed"` when from real-time chain observations.
    /// Clients should not assume `"observed"` is more accurate (the
    /// estimate is empirically within 10% of observed on Mocha) but
    /// MAY display the source for transparency.
    source: String,
    /// RFC3339 UTC instant the values were computed.
    as_of: String,
}

pub async fn finality(State(state): State<AppState>) -> Response {
    let key = "finality";
    if let Some(cached) = state.stats_cache.get_fresh(key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    // Estimated values for Mocha. p50 = ~one Mocha block (~12s); p95
    // and p99 trend to the next-block deadline (~15s, matches the
    // operator dashboard's `ligate_da_finalization_latency_seconds`
    // histogram during the devnet-1 smoke test).
    let payload = FinalityResponse {
        window: "static".to_string(),
        sampled_count: 0,
        p50_seconds: 12.0,
        p95_seconds: 15.0,
        p99_seconds: 15.0,
        da_layer: "celestia-mocha".to_string(),
        source: "estimated".to_string(),
        as_of: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    match serde_json::to_string(&payload) {
        Ok(body) => {
            state.stats_cache.put(key.to_string(), body.clone());
            cached_json_response(body)
        }
        Err(e) => error_response(e.into()),
    }
}

// ---- /top-holders ----------------------------------------------------------

#[derive(Deserialize)]
pub struct TopHoldersQuery {
    /// Number of holders to return. Default 10, capped at 100.
    #[serde(default)]
    n: Option<u32>,
}

#[derive(Serialize)]
struct TopHolder {
    rank: u32,
    address: String,
    /// String form to safely cross u128/JSON-number boundaries.
    balance_nano: String,
    /// `balance_nano / 1e9` rendered as f64; lossy beyond ~2^53 nano.
    balance_lgt: f64,
}

#[derive(Serialize)]
struct TopHoldersResponse {
    /// Source of the balance numbers: `"chain"` means queried live from
    /// the chain's bank module. Future versions may add an indexed
    /// `balance_nano` column on `address_summaries` and report
    /// `"indexer"` here without changing the wire shape.
    source: String,
    holders: Vec<TopHolder>,
}

pub async fn top_holders(
    State(state): State<AppState>,
    Query(params): Query<TopHoldersQuery>,
) -> Response {
    let n = params.n.unwrap_or(10).min(100);
    let key = format!("top-holders:{n}");
    if let Some(cached) = state.stats_cache.get_fresh(&key, DEFAULT_TTL) {
        return cached_json_response(cached);
    }
    match compute_top_holders(&state, n).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_top_holders(state: &AppState, n: u32) -> anyhow::Result<TopHoldersResponse> {
    // Pull the candidate address universe from the indexer. We cap
    // at 1000 to bound chain-RPC load: devnet has <100 addresses
    // total, mainnet eventually needs a real `balance_nano` index
    // (TODO: indexer migration); this query is a stopgap.
    let addresses: Vec<String> =
        sqlx::query_scalar("SELECT address FROM address_summaries LIMIT 1000")
            .fetch_all(&state.pg)
            .await
            .context("address-summary list")?;
    let mut with_balance: Vec<(String, u128)> = Vec::with_capacity(addresses.len());
    for addr in addresses {
        match state.signer.query_balance_for(&addr).await {
            Ok(bal) => with_balance.push((addr, bal)),
            Err(e) => {
                // Best-effort; an unreachable account or RPC blip
                // shouldn't kill the whole endpoint.
                tracing::debug!(address = %addr, error = ?e, "balance query failed; skipping");
            }
        }
    }
    with_balance.sort_by(|a, b| b.1.cmp(&a.1));
    with_balance.truncate(n as usize);
    let holders = with_balance
        .into_iter()
        .enumerate()
        .map(|(i, (address, balance_nano))| TopHolder {
            rank: (i as u32) + 1,
            address,
            balance_nano: balance_nano.to_string(),
            balance_lgt: (balance_nano as f64) / 1_000_000_000.0,
        })
        .collect();
    Ok(TopHoldersResponse {
        source: "chain".to_string(),
        holders,
    })
}

// ---- Helpers ---------------------------------------------------------------

/// Parse a duration string like `24h`, `7d`, `1h`, `30m` into a
/// `Duration`. Subset of humantime; we control the inputs so we keep
/// the grammar minimal.
fn parse_interval(s: &str) -> anyhow::Result<Duration> {
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| anyhow::anyhow!("window '{s}' missing unit (h/d/m/s)"))?,
    );
    let n: u64 = num
        .parse()
        .with_context(|| format!("window '{s}': expected leading integer"))?;
    let seconds = match unit {
        "s" => n,
        "m" => n.checked_mul(60).context("overflow")?,
        "h" => n.checked_mul(3600).context("overflow")?,
        "d" => n.checked_mul(86400).context("overflow")?,
        other => anyhow::bail!("window '{s}': unknown unit '{other}' (expected s/m/h/d)"),
    };
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intervals() {
        assert_eq!(parse_interval("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_interval("7d").unwrap(), Duration::from_secs(604_800));
    }

    #[test]
    fn rejects_bad_intervals() {
        assert!(parse_interval("24").is_err()); // no unit
        assert!(parse_interval("h").is_err()); // no number
        assert!(parse_interval("5y").is_err()); // unknown unit
    }
}
