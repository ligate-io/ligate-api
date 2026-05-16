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

/// Default downstream Cache-Control TTL for stats endpoints whose
/// data changes on a slow rolling window (active addresses, daily
/// timeseries, top-holders, finality percentiles). Matches the
/// in-process `DEFAULT_TTL` so cache invalidation aligns.
const TTL_STATS_DEFAULT_SECS: u32 = 30;

/// `/v1/stats/totals` — counts move every block but the explorer
/// re-renders often; 10s is the sweet spot between "data freshness"
/// and "actually cacheable downstream."
const TTL_STATS_TOTALS_SECS: u32 = 10;

/// `/v1/stats/next-block-eta` — by definition changes every second.
/// 5s downstream TTL matches the in-process cache TTL set in the
/// handler; documented in the explorer perf brief (api#48).
const TTL_STATS_NEXT_BLOCK_ETA_SECS: u32 = 5;

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
/// response with a per-endpoint `max_age_secs`. Centralised so
/// adding headers (eg. `Vary`, `ETag`) later only touches one site.
///
/// Per-endpoint TTLs are chosen to match the endpoint's actual
/// volatility — `/v1/info` changes per-block (5s), historical
/// percentiles change slowly (30s), block-history doesn't change
/// at all (300s+). Next.js + Vercel CDN honor these automatically
/// downstream.
fn cached_json_response(body: String, max_age_secs: u32) -> Response {
    let cache_control = format!("public, max-age={max_age_secs}");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, cache_control.as_str()),
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
        return cached_json_response(cached, TTL_STATS_TOTALS_SECS);
    }
    match compute_totals(&state).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key.to_string(), body.clone());
                cached_json_response(body, TTL_STATS_TOTALS_SECS)
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
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    match compute_active_addresses(&state.pg, &window, interval).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body, TTL_STATS_DEFAULT_SECS)
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
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    match compute_new_wallets_daily(&state.pg, days).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body, TTL_STATS_DEFAULT_SECS)
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

// ---- /attestations-daily ---------------------------------------------------

#[derive(Deserialize)]
pub struct AttestationsDailyQuery {
    /// How many days of history to return. Default 30 (matches the
    /// explorer's GitHub-style heatmap grid), capped at 90.
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Serialize)]
struct AttestationsDailyResponse {
    days: u32,
    points: Vec<DailyPoint>,
}

/// `GET /v1/stats/attestations-daily?days=N` — daily count of
/// attestations submitted, bucketed by UTC day, for the trailing
/// N days. Powers the explorer's "DAILY ATTESTATIONS" heatmap
/// (default 30-day window) so it doesn't have to filter the broader
/// `/v1/stats/tx-rate-daily` response client-side.
///
/// Same `DailyPoint` shape as `/v1/stats/new-wallets-daily` so
/// callers can reuse rendering helpers across both endpoints.
pub async fn attestations_daily(
    State(state): State<AppState>,
    Query(params): Query<AttestationsDailyQuery>,
) -> Response {
    let days = params.days.unwrap_or(30).min(90);
    let key = format!("attestations-daily:{days}");
    if let Some(cached) = state.stats_cache.get_fresh(&key, DEFAULT_TTL) {
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    match compute_attestations_daily(&state.pg, days).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body, TTL_STATS_DEFAULT_SECS)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_attestations_daily(
    pool: &PgPool,
    days: u32,
) -> anyhow::Result<AttestationsDailyResponse> {
    let since = Utc::now() - chrono::Duration::days(days as i64);
    // Same bucket-by-UTC-day shape as `compute_new_wallets_daily`,
    // just against the `attestations` table's `submitted_at_timestamp`.
    // Days with zero attestations are absent from the rows (Postgres
    // `GROUP BY` doesn't emit empty groups); the explorer fills the
    // 30-cell grid with 0s for missing dates.
    let rows: Vec<(DateTime<Utc>, i64)> = sqlx::query_as(
        "SELECT DATE_TRUNC('day', submitted_at_timestamp) AS day, COUNT(*) AS attestations \
         FROM attestations \
         WHERE submitted_at_timestamp >= $1 \
         GROUP BY day \
         ORDER BY day ASC",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .context("daily attestations bucket")?;
    let points = rows
        .into_iter()
        .map(|(day, count)| DailyPoint {
            date: day.format("%Y-%m-%d").to_string(),
            count,
        })
        .collect();
    Ok(AttestationsDailyResponse { days, points })
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
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    match compute_tx_rate_daily(&state.pg, days).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body, TTL_STATS_DEFAULT_SECS)
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
/// **Now backed by observations.** Starting with migration 0006,
/// the indexer stamps `slots.finalized_at` (wall-clock NOW) when
/// it observes the chain's per-slot `finality_status` field
/// transition `pending → finalized`. This endpoint reads those
/// observations directly: `finalized_at - timestamp` is the
/// observed finalization latency per slot. We aggregate over the
/// last 1h.
///
/// **Observation lag.** The indexer's re-poll loop runs every
/// `FINALITY_REPOLL_INTERVAL` (10s), so observed values can be up
/// to 10s above the true chain finalization moment. Acceptable
/// v0 fidelity; a tighter measurement would require subscribing
/// to the chain's `BlobExecutionStatus` broadcast channel, which
/// isn't reachable from the api's network in current deploy
/// topology. Tracked as a followup.
///
/// **Fallback.** When the indexer hasn't yet observed any
/// transitions (fresh deploy, head < migration-0006 + few
/// pending slots), this endpoint falls back to the previous
/// hardcoded Mocha-derived estimates (~12s p50, ~15s p95/p99)
/// and sets `source: "estimated"` so clients can render an
/// appropriate label.
#[derive(Serialize)]
struct FinalityResponse {
    /// Window the percentiles are computed over. `"static"` when
    /// `source == "estimated"`; `"1h"` when `source == "observed"`.
    window: String,
    /// Number of finalization observations in the window. `0`
    /// while `source == "estimated"`. Clients should treat
    /// `sampled_count < ~20` with reduced confidence.
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
    /// model (insufficient samples in the window); `"observed"`
    /// when from real-time indexer observations. Clients SHOULD
    /// display this distinction.
    source: String,
    /// RFC3339 UTC instant the values were computed.
    as_of: String,
}

/// Below this sample count we don't trust the observed
/// percentiles and fall back to the static estimate. Set high
/// enough that a quiet hour (few finalizations) doesn't render
/// noisy outliers as "real" stats, but low enough that the
/// observed mode kicks in within an hour of normal operation
/// (Mocha produces ~600 slots/hour → 600 observations once
/// backfill catches up).
const FINALITY_MIN_SAMPLES: i64 = 20;

pub async fn finality(State(state): State<AppState>) -> Response {
    let key = "finality";
    if let Some(cached) = state.stats_cache.get_fresh(key, DEFAULT_TTL) {
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    let payload = match compute_observed_finality(&state.pg).await {
        Ok(p) => p,
        Err(e) => {
            // Treat DB errors as fall-through to the estimate.
            // Better to keep the endpoint serving stable data than
            // 500 the explorer when sampling has a transient hiccup.
            tracing::warn!(error = %e, "finality observed sampling; falling back to estimate");
            estimated_finality()
        }
    };
    match serde_json::to_string(&payload) {
        Ok(body) => {
            state.stats_cache.put(key.to_string(), body.clone());
            cached_json_response(body, TTL_STATS_DEFAULT_SECS)
        }
        Err(e) => error_response(e.into()),
    }
}

/// Fallback values used when observed sampling has too few rows
/// to be trustworthy. Same numbers we shipped pre-migration-0006.
fn estimated_finality() -> FinalityResponse {
    FinalityResponse {
        window: "static".to_string(),
        sampled_count: 0,
        p50_seconds: 12.0,
        p95_seconds: 15.0,
        p99_seconds: 15.0,
        da_layer: "celestia-mocha".to_string(),
        source: "estimated".to_string(),
        as_of: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

/// Run the percentile query against `slots.finalized_at -
/// slots.timestamp` for the last 1h. Returns the estimated
/// fallback if `sampled_count < FINALITY_MIN_SAMPLES`.
///
/// **SQL note.** We use `percentile_cont` (continuous interpolation)
/// rather than `percentile_disc` because finalization latency is a
/// continuous quantity — half a slot of interpolation is more
/// faithful than rounding to the nearest sampled value.
/// Tuple shape returned by the `compute_observed_finality` SQL.
/// `Option<f64>` for the three percentiles because `percentile_cont`
/// returns NULL when the input set is empty (Postgres-side); `i64`
/// for the count is non-null because COUNT(*) is always defined.
type FinalityRow = (Option<f64>, Option<f64>, Option<f64>, i64);

async fn compute_observed_finality(pool: &PgPool) -> anyhow::Result<FinalityResponse> {
    // `EXTRACT(EPOCH FROM (a - b))` returns the interval as float
    // seconds, including sub-second precision. The `WHERE` filter
    // keeps the window honest (excluding NULL `finalized_at` rows
    // is implicit in the inequality).
    let row: Option<FinalityRow> = sqlx::query_as(
        "SELECT
             percentile_cont(0.5)  WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (finalized_at - timestamp))),
             percentile_cont(0.95) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (finalized_at - timestamp))),
             percentile_cont(0.99) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (finalized_at - timestamp))),
             COUNT(*)
         FROM slots
         WHERE finalized_at IS NOT NULL
           AND finalized_at > NOW() - INTERVAL '1 hour'",
    )
    .fetch_optional(pool)
    .await
    .context("running finality percentile query")?;

    let Some((p50, p95, p99, count)) = row else {
        return Ok(estimated_finality());
    };
    if count < FINALITY_MIN_SAMPLES {
        return Ok(estimated_finality());
    }
    // Once `count >= FINALITY_MIN_SAMPLES`, the percentile_cont
    // outputs should be Some. Guard with `unwrap_or` as defensive
    // programming — preferred fallback is the estimate, not 0.0
    // which would be misleading.
    let p50 = p50.unwrap_or(12.0);
    let p95 = p95.unwrap_or(15.0);
    let p99 = p99.unwrap_or(15.0);

    Ok(FinalityResponse {
        window: "1h".to_string(),
        sampled_count: count as u32,
        p50_seconds: p50,
        p95_seconds: p95,
        p99_seconds: p99,
        da_layer: "celestia-mocha".to_string(),
        source: "observed".to_string(),
        as_of: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

// ---- /next-block-eta -------------------------------------------------------

/// Live block-cadence response, used by explorers to render
/// "next block in Ns" countdowns. Computed from the last N
/// observed slots; updates every cache refresh.
///
/// The countdown the explorer renders is `seconds_until_expected`
/// minus the time elapsed since `as_of`. Frontend should locally
/// tick once per second and re-fetch this endpoint either on a
/// timer (~10s) or when the countdown rolls negative.
#[derive(Serialize)]
struct NextBlockEtaResponse {
    /// Most recent block the indexer has committed.
    last_block_height: i64,
    /// RFC3339 ms UTC timestamp of the most recent block.
    last_block_timestamp: String,
    /// Mean wall-clock interval between consecutive slots over the
    /// sample window. Float seconds. `None` if fewer than 2 slots
    /// have been indexed (indexer just started, no delta to compute).
    #[serde(skip_serializing_if = "Option::is_none")]
    mean_block_interval_secs: Option<f64>,
    /// p95 of the same interval distribution. `None` under-2-slots
    /// for the same reason as above.
    #[serde(skip_serializing_if = "Option::is_none")]
    p95_block_interval_secs: Option<f64>,
    /// RFC3339 ms UTC timestamp the next block is expected at, equal
    /// to `last_block_timestamp + mean_block_interval`. `None` if we
    /// can't compute the interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_next_at: Option<String>,
    /// Seconds elapsed since `last_block_timestamp` at request time.
    /// Computed server-side so client clocks don't matter.
    seconds_since_last: f64,
    /// `expected_next_at - now`. Negative when overdue (block should
    /// have arrived but hasn't). `None` when we can't compute the
    /// interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds_until_expected: Option<f64>,
    /// How far the indexer is behind the actual chain head, in
    /// seconds. Computed as `(chain_head_height - last_indexed_height)
    /// × mean_block_interval_secs`. `0.0` for an indexer at the tail
    /// or when chain-head fetch failed (silent — see warning log).
    ///
    /// **Threshold semantics for client UX.** A healthy indexer at
    /// the tail reports < `mean_block_interval_secs` here (one slot
    /// of natural in-flight between chain producing and indexer
    /// committing). Anything above ~3× mean is a genuine "indexer is
    /// behind" signal worth surfacing to users. Avoid using this
    /// value as a real-time countdown — for that, use
    /// `seconds_since_last` instead.
    indexer_lag_secs: f64,
}

pub async fn next_block_eta(State(state): State<AppState>) -> Response {
    let key = "next-block-eta";
    // Cache TTL shorter than the other stats endpoints (5s vs 30s)
    // because this is the one query whose answer literally changes
    // every second the chain is alive.
    let ttl = Duration::from_secs(5);
    if let Some(cached) = state.stats_cache.get_fresh(key, ttl) {
        return cached_json_response(cached, TTL_STATS_NEXT_BLOCK_ETA_SECS);
    }
    // Fetch the chain's head height in parallel with the slot-history
    // query inside `compute_next_block_eta` so `indexer_lag_secs`
    // reports a true (chain_head - indexer_head) signal rather than
    // the previous "seconds since last block" surrogate that cycled
    // 0 → mean-interval every block regardless of whether the
    // indexer was actually behind.
    let chain_head = match ligate_api_indexer::NodeClient::new(&state.config.chain_rpc) {
        Ok(client) => client.latest_slot().await.ok().map(|s| s.number),
        Err(e) => {
            tracing::warn!(error = %e, "building NodeClient in /v1/stats/next-block-eta");
            None
        }
    };
    match compute_next_block_eta(&state.pg, chain_head).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key.to_string(), body.clone());
                cached_json_response(body, TTL_STATS_NEXT_BLOCK_ETA_SECS)
            }
            Err(e) => error_response(e.into()),
        },
        Err(e) => error_response(e),
    }
}

async fn compute_next_block_eta(
    pool: &PgPool,
    chain_head: Option<u64>,
) -> anyhow::Result<NextBlockEtaResponse> {
    // Last 100 slots, descending. Window matched to "last ~10 min
    // on Mocha" (block time ~6s; 100 * 6 = 600s). Enough samples
    // for a stable mean + p95 without being a wide query.
    let rows: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT height, timestamp \
         FROM slots \
         ORDER BY height DESC \
         LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .context("fetching recent slots for next-block-eta")?;

    if rows.is_empty() {
        // No slots indexed at all. Honest empty response; explorer
        // can fall back to a "indexer warming up" UI.
        anyhow::bail!("no slots indexed yet");
    }

    let (latest_height, latest_ts) = rows[0];
    let now = Utc::now();
    let seconds_since_last = (now - latest_ts).num_milliseconds() as f64 / 1000.0;

    // For the interval sample we need at least 2 slots. With 1 slot
    // we can still return the last-block fields and lag, but the
    // ETA prediction is `None`.
    let (mean_interval, p95_interval, expected_next_at, seconds_until_expected) = if rows.len() >= 2
    {
        // Pairwise deltas: rows is DESC by height, so `rows[i] - rows[i+1]`
        // is the wall-clock seconds the (i+1) → i transition took.
        let mut deltas: Vec<f64> = rows
            .windows(2)
            .filter_map(|w| {
                let (newer_ts, older_ts) = (w[0].1, w[1].1);
                let delta = (newer_ts - older_ts).num_milliseconds() as f64 / 1000.0;
                // Defensive: drop non-positive deltas (would be a
                // re-org or clock skew) so they don't poison the
                // mean. Realistic devnet should never hit this.
                if delta > 0.0 {
                    Some(delta)
                } else {
                    None
                }
            })
            .collect();
        if deltas.is_empty() {
            (None, None, None, None)
        } else {
            let mean = deltas.iter().sum::<f64>() / (deltas.len() as f64);
            // p95 via simple-sort percentile. Sample is at most 99
            // values so the O(n log n) sort cost is negligible vs
            // the SQL round-trip.
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p95_idx = ((deltas.len() as f64) * 0.95).ceil() as usize - 1;
            let p95 = deltas[p95_idx.min(deltas.len() - 1)];
            let expected_at = latest_ts + chrono::Duration::milliseconds((mean * 1000.0) as i64);
            let until_expected = (expected_at - now).num_milliseconds() as f64 / 1000.0;
            (
                Some(mean),
                Some(p95),
                Some(expected_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                Some(until_expected),
            )
        }
    } else {
        (None, None, None, None)
    };

    // Indexer-lag: true `(chain_head - last_indexed_height)` × mean
    // interval. `chain_head` is fetched in parallel by the caller. If
    // either the chain-head fetch failed or we don't yet have a mean
    // interval (single-slot indexer), report `0.0` rather than
    // surfacing the previous "seconds since last block" surrogate
    // that cycled 0 → mean every block. A healthy indexer at the
    // chain tail reports < ~mean here (one slot of natural in-flight);
    // a genuinely lagging indexer reports `N × mean` where N is the
    // backlog in slots.
    let indexer_lag_secs = match (chain_head, mean_interval) {
        (Some(head), Some(mean)) => {
            let behind = (head as i64 - latest_height).max(0) as f64;
            behind * mean
        }
        _ => 0.0,
    };

    Ok(NextBlockEtaResponse {
        last_block_height: latest_height,
        last_block_timestamp: latest_ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        mean_block_interval_secs: mean_interval,
        p95_block_interval_secs: p95_interval,
        expected_next_at,
        seconds_since_last,
        seconds_until_expected,
        indexer_lag_secs,
    })
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
        return cached_json_response(cached, TTL_STATS_DEFAULT_SECS);
    }
    match compute_top_holders(&state, n).await {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(body) => {
                state.stats_cache.put(key, body.clone());
                cached_json_response(body, TTL_STATS_DEFAULT_SECS)
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
