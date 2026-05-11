//! HTTP handlers.
//!
//! Three endpoint families:
//!
//! - **Operator probes** — `/health`, `/v1/health`. Always 200; no
//!   chain or DB queries.
//! - **Drip (faucet)** — `POST /v1/drip`, `GET /v1/drip/status`.
//!   Rate-limited per-address.
//! - **Indexer queries** — `GET /v1/blocks*`, `/v1/txs*`,
//!   `/v1/addresses/*`, `/v1/schemas*`, `/v1/info`. Read from Postgres
//!   (which the indexer task writes). v0 ships these as placeholders;
//!   subsequent PRs flesh out the schemas + queries.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use ligate_api_drip::{RateCheck, SignerError};
use serde::{Deserialize, Serialize};

use crate::AppState;

// ---- Operator probes -------------------------------------------------------

pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

// ---- Drip ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DripRequest {
    pub address: String,
}

#[derive(Debug, Serialize)]
pub struct DripResponse {
    /// Bech32m `lig1...` address that was funded (echoed back verbatim
    /// from the `DripRequest.address` field).
    pub address: String,
    /// Transaction hash from the chain's submit endpoint. Bech32m with
    /// HRP `ltx` (`ltx1...`) as of `ligate-chain@0ac7e5b`; previously
    /// raw hex. The faucet forwards whatever the chain's tx-hash
    /// `Display` impl returns at runtime, so the format follows
    /// whichever chain rev the faucet is pointed at without code
    /// changes.
    pub tx_hash: String,
    /// Amount dripped, in nano-LGT (u128 fits in JSON number for the
    /// `1e9 * default_drip` values we use). Decimal-string preferred
    /// over numbers per RFC 0002 for amounts >2^53, but the drip
    /// default sits well below that ceiling.
    pub amount_nano: u128,
    /// Convenience `amount_nano / 1e9` rendered as a float for human
    /// display; not for accounting. Source of truth is `amount_nano`.
    pub drip_amount_lgt: f64,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub retry_after_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct DripStatusResponse {
    pub drip_amount_nano: u128,
    pub drip_amount_lgt: f64,
    pub rate_limit_secs: u64,
    pub addresses_dripped: usize,
    pub faucet_address: String,
}

pub async fn drip_status(State(state): State<AppState>) -> impl IntoResponse {
    let drip_nano = state.config.drip_amount;
    Json(DripStatusResponse {
        drip_amount_nano: drip_nano,
        drip_amount_lgt: nano_to_lgt(drip_nano),
        rate_limit_secs: state.config.drip_rate_limit_secs,
        addresses_dripped: state.rate_limiter.drip_count(),
        faucet_address: state.signer.address(),
    })
}

pub async fn drip(
    State(state): State<AppState>,
    Json(req): Json<DripRequest>,
) -> Result<Json<DripResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Rate-limit check BEFORE we touch the signer.
    match state.rate_limiter.check(&req.address) {
        RateCheck::Allowed => {}
        RateCheck::Blocked { retry_after } => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse {
                    error: format!(
                        "address rate-limited; retry in {} seconds",
                        retry_after.as_secs()
                    ),
                    retry_after_secs: Some(retry_after.as_secs()),
                }),
            ));
        }
    }

    // 2. Sign + submit.
    let receipt = state
        .signer
        .drip(&req.address, state.config.drip_amount)
        .await
        .map_err(|e| match e {
            SignerError::InvalidAddress(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    retry_after_secs: None,
                }),
            ),
            SignerError::InvalidSignerKey(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("operator misconfig: {msg}"),
                    retry_after_secs: None,
                }),
            ),
            SignerError::SubmitFailed(msg) => (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("chain submission failed: {msg}"),
                    retry_after_secs: None,
                }),
            ),
        })?;

    // 3. Record AFTER the chain accepted (failed submits don't burn
    //    the address's window).
    state.rate_limiter.record(&req.address);

    Ok(Json(DripResponse {
        address: req.address,
        tx_hash: receipt.tx_hash,
        amount_nano: receipt.amount_nano,
        drip_amount_lgt: nano_to_lgt(receipt.amount_nano),
    }))
}

// ---- Indexer query endpoints (v0 placeholders) -----------------------------
//
// The indexer's Postgres schema is still being shaped (slots only at
// the moment; tx / schema / attestation tables come in subsequent PRs).
// Until those land, these handlers return 501 NOT_IMPLEMENTED with a
// pointer at the tracking issue. The explorer's Next.js frontend
// renders mock data behind a `USE_MOCK_API=true` env var — flip to
// `false` once a given endpoint is real.

/// Pagination shape used by every list endpoint. Currently inert
/// because v0 indexer query handlers all return 501; the fields will
/// drive Postgres `LIMIT` + cursor pagination once the queries land.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PaginationParams {
    pub limit: Option<u32>,
    pub before: Option<String>,
}

pub async fn info(State(_state): State<AppState>) -> impl IntoResponse {
    // Proxy to the chain's `/v1/rollup/info`. Wired in subsequent PR;
    // v0 returns a placeholder so the explorer's `/info` route doesn't
    // 500 in mock-API mode.
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "not implemented yet",
            "tracking": "https://github.com/ligate-io/ligate-api/issues/1"
        })),
    )
}

pub async fn blocks_list(
    State(_state): State<AppState>,
    Query(_params): Query<PaginationParams>,
) -> impl IntoResponse {
    not_implemented("/v1/blocks list")
}

pub async fn block_by_height(
    State(_state): State<AppState>,
    Path(_height): Path<u64>,
) -> impl IntoResponse {
    not_implemented("/v1/blocks/{height}")
}

pub async fn txs_list(
    State(_state): State<AppState>,
    Query(_params): Query<PaginationParams>,
) -> impl IntoResponse {
    not_implemented("/v1/txs list")
}

pub async fn tx_by_hash(
    State(_state): State<AppState>,
    Path(_hash): Path<String>,
) -> impl IntoResponse {
    not_implemented("/v1/txs/{hash}")
}

pub async fn address_summary(
    State(_state): State<AppState>,
    Path(_addr): Path<String>,
) -> impl IntoResponse {
    not_implemented("/v1/addresses/{addr}")
}

pub async fn schemas_list(
    State(_state): State<AppState>,
    Query(_params): Query<PaginationParams>,
) -> impl IntoResponse {
    not_implemented("/v1/schemas list")
}

pub async fn schema_by_id(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    not_implemented("/v1/schemas/{id}")
}

pub async fn attestor_set_by_id(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    not_implemented("/v1/attestor-sets/{id}")
}

fn not_implemented(endpoint: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": format!("{endpoint} is not implemented yet"),
            "tracking": "https://github.com/ligate-io/ligate-api/issues/1"
        })),
    )
}

// ---- helpers ---------------------------------------------------------------

fn nano_to_lgt(nano: u128) -> f64 {
    (nano as f64) / 1_000_000_000.0
}
