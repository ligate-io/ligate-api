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
use chrono::SecondsFormat;
use ligate_api_drip::{RateCheck, SignerError};
use ligate_api_indexer::NodeClient;
use serde::{Deserialize, Serialize};

use crate::cursor;
use crate::queries;
use crate::responses::{
    AddressSummaryResponse, BlockResponse, InfoResponse, Page, Pagination, SeenAtResponse,
    TxResponse,
};
use crate::AppState;

/// Cursor shape for `/v1/blocks` (descending by slot height).
#[derive(Debug, Serialize, Deserialize)]
struct BlocksCursor {
    slot: u64,
}

/// Cursor shape for `/v1/txs` (compound: descending by (slot, idx)).
/// `idx` matches the `transactions.position` column on the read side.
#[derive(Debug, Serialize, Deserialize)]
struct TxsCursor {
    slot: u64,
    idx: u32,
}

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

// ---- Indexer query endpoints -----------------------------------------------
//
// Read directly from Postgres tables the indexer task keeps current.
// Response shapes pinned by RFC 0002 (`docs/rfc/0002-response-shapes.md`);
// pagination shapes pinned by RFC 0001. Endpoints not yet wired (txs,
// schemas, addresses, attestor-sets) still return 501 with the
// tracking-issue pointer below.

/// Pagination query string shared by every list endpoint.
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    pub limit: Option<u32>,
    pub before: Option<String>,
}

/// `GET /v1/info` — chain identity + indexer head.
///
/// Proxies `GET /v1/rollup/info` from the chain, augments with the
/// indexer's `MAX(height)` from Postgres. If either side fails, the
/// fields that depend on it come back `null` rather than 502'ing the
/// whole response — chain identity is independent of indexer health,
/// and the explorer's "catching up" badge would rather render with
/// partial data than show nothing.
pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    // Indexer head — local, fast, infallible enough to swallow errors.
    let indexer_height = match queries::max_slot_height(&state.pg).await {
        Ok(h) => h.map(|i| i as u64),
        Err(e) => {
            tracing::warn!(error = %e, "max_slot_height in /v1/info");
            None
        }
    };

    // Chain proxy — same RPC URL the indexer uses. Reusing the
    // indexer's NodeClient keeps the URL-shaping consistent.
    let chain_info = match NodeClient::new(&state.config.chain_rpc) {
        Ok(client) => client.rollup_info().await.ok(),
        Err(e) => {
            tracing::warn!(error = %e, "building NodeClient in /v1/info");
            None
        }
    };

    let (chain_id, chain_hash, version, head_height) = match chain_info {
        Some(info) => (
            info.chain_id,
            info.chain_hash,
            info.version,
            // The chain's `/v1/rollup/info` doesn't expose head
            // height directly; the indexer task's bootstrap reads it
            // via a separate `/v1/ledger/slots/latest` call. To keep
            // this handler one-roundtrip, surface `head_height` as
            // `indexer_height` for now — they're equal at the
            // tail-poll cadence (which is the common case). A
            // follow-up can split the two via a parallel proxy call
            // if observable lag becomes a real symptom.
            indexer_height,
        ),
        None => (String::new(), String::new(), String::new(), None),
    };

    let head_lag_slots = match (head_height, indexer_height) {
        (Some(head), Some(at)) => Some(head.saturating_sub(at)),
        _ => None,
    };

    Json(InfoResponse {
        chain_id,
        chain_hash,
        version,
        indexer_height,
        head_height,
        head_lag_slots,
    })
    .into_response()
}

/// `GET /v1/blocks` — descending list of slots with cursor pagination.
///
/// Reads from the `slots` table. Each row maps to a
/// [`BlockResponse`] per RFC 0002 (height, hash, parent_hash,
/// timestamp, tx_count, etc.). Cursor is opaque base64url-encoded
/// JSON of the last row's slot height.
pub async fn blocks_list(
    State(state): State<AppState>,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let limit = cursor::resolve_limit(params.limit);
    let before_height: Option<i64> = params
        .before
        .as_deref()
        .and_then(cursor::decode::<BlocksCursor>)
        .map(|c| c.slot as i64);

    // Fetch one extra to know whether a `next` cursor is warranted
    // without a separate count query.
    let limit_plus_one = (limit as i64) + 1;
    let mut rows = match queries::slots_page(&state.pg, before_height, limit_plus_one).await {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!(error = %e, "slots_page in /v1/blocks");
            return internal_error();
        }
    };

    let has_more = rows.len() as i64 > limit as i64;
    if has_more {
        rows.truncate(limit as usize);
    }

    let next = if has_more {
        rows.last().and_then(|r| {
            cursor::encode(&BlocksCursor {
                slot: r.height as u64,
            })
            .ok()
        })
    } else {
        None
    };

    let data: Vec<BlockResponse> = rows.into_iter().map(slot_to_block_response).collect();

    Json(Page {
        data,
        pagination: Pagination { next, limit },
    })
    .into_response()
}

/// `GET /v1/blocks/{height}` — a single slot by height.
///
/// Returns 404 with the standard error envelope when the indexer
/// hasn't written that height yet (either it's above the chain head,
/// or the indexer is behind). Distinguishing those two cases needs a
/// chain-side head query; for v0 the unified 404 is fine and matches
/// the "indexer is the source of truth for this surface" framing.
pub async fn block_by_height(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> impl IntoResponse {
    let row = match queries::slot_by_height(&state.pg, height as i64).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "block not found",
                    "tracking": null
                })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, height, "slot_by_height in /v1/blocks/{{height}}");
            return internal_error();
        }
    };

    Json(slot_to_block_response(row)).into_response()
}

/// Map a Postgres row from the `slots` table to the RFC 0002
/// `Block` wire shape. Bridges the Postgres-side typing (i64
/// heights, `chrono::DateTime<Utc>` timestamps) to the JSON
/// representation (JSON number heights, RFC3339-millis timestamps).
fn slot_to_block_response(row: queries::SlotRow) -> BlockResponse {
    BlockResponse {
        height: row.height as u64,
        hash: row.hash,
        parent_hash: row.prev_hash,
        state_root: row.state_root,
        timestamp: row.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
        tx_count: row.tx_count,
        batch_count: row.batch_count,
        // Reserved — chain doesn't emit these in v0. See
        // BlockResponse field docs for rationale.
        proposer: None,
        size_bytes: None,
    }
}

fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "internal error",
            "tracking": null
        })),
    )
        .into_response()
}

/// `GET /v1/txs` — descending list of indexed transactions with
/// compound cursor pagination.
///
/// Reads from `transactions ⨝ slots`, ordered by `(slot DESC,
/// position DESC)`. Cursor encodes the last row's `(slot, idx)`.
pub async fn txs_list(
    State(state): State<AppState>,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let limit = cursor::resolve_limit(params.limit);
    let before = params
        .before
        .as_deref()
        .and_then(cursor::decode::<TxsCursor>)
        .map(|c| queries::TxsCursor {
            slot: c.slot as i64,
            position: c.idx as i32,
        });

    let limit_plus_one = (limit as i64) + 1;
    let mut rows = match queries::txs_page(&state.pg, before, limit_plus_one).await {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!(error = %e, "txs_page in /v1/txs");
            return internal_error();
        }
    };

    let has_more = rows.len() as i64 > limit as i64;
    if has_more {
        rows.truncate(limit as usize);
    }

    let next = if has_more {
        rows.last().and_then(|r| {
            cursor::encode(&TxsCursor {
                slot: r.slot as u64,
                idx: r.position as u32,
            })
            .ok()
        })
    } else {
        None
    };

    let data: Vec<TxResponse> = rows.into_iter().map(tx_row_to_response).collect();

    Json(Page {
        data,
        pagination: Pagination { next, limit },
    })
    .into_response()
}

/// `GET /v1/txs/{hash}` — a single tx by hash.
///
/// 404 when the indexer hasn't written the hash. The chain may have
/// emitted it pre-finality, the partner may have a typo, or the tx
/// genuinely doesn't exist; we don't try to distinguish.
pub async fn tx_by_hash(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> impl IntoResponse {
    let row = match queries::tx_by_hash(&state.pg, &hash).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "tx not found",
                    "tracking": null
                })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, %hash, "tx_by_hash in /v1/txs/{{hash}}");
            return internal_error();
        }
    };

    Json(tx_row_to_response(row)).into_response()
}

/// Map a Postgres row from the `transactions ⨝ slots` join to the
/// RFC 0002 `Tx` wire shape.
fn tx_row_to_response(row: queries::TxRow) -> TxResponse {
    TxResponse {
        hash: row.hash,
        block_height: row.slot as u64,
        block_hash: row.block_hash,
        block_timestamp: row
            .block_timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        position: row.position,
        sender: row.sender,
        sender_pubkey: row.sender_pubkey,
        nonce: row.nonce,
        fee_paid_nano: row.fee_paid_nano,
        kind: row.kind,
        details: row.details,
        outcome: row.outcome,
        revert_reason: row.revert_reason,
    }
}

/// `GET /v1/addresses/{addr}` — per-address activity summary.
///
/// Indexer-side counters + first/last seen come from the
/// `address_summaries` Postgres table. Live balances are NOT in
/// the indexer — they'd be stale by definition; the handler proxies
/// chain RPC at read time.
///
/// v0 limitation: only the gas token balance (`config_gas_token_id`)
/// is surfaced. Multi-token expansion is a follow-up — needs either
/// a chain endpoint that lists all tokens an address holds (none
/// today) or an indexer-side per-(address, token) ledger derived
/// from `Bank/TokenTransferred` events (Phase F.2, not in this PR).
pub async fn address_summary(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> impl IntoResponse {
    // Indexer-side counters. Zero-row for unknown addresses (still
    // a valid shape, just zeros).
    let row = match queries::address_summary(&state.pg, &addr).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, %addr, "address_summary in /v1/addresses/{{addr}}");
            return internal_error();
        }
    };

    // Live gas-token balance via the drip signer's NodeClient (it
    // already knows the LGT token id from boot config). On failure,
    // surface an empty balances vec rather than 502'ing — counters
    // are still useful even when chain RPC is briefly down.
    let balances = match state.signer.query_balance_for(&addr).await {
        Ok(b) => vec![crate::responses::TokenBalanceResponse {
            token_id: state.signer.lgt_token_id_bech32(),
            amount_nano: b.to_string(),
        }],
        Err(e) => {
            tracing::warn!(error = %e, %addr, "chain balance lookup failed; surfacing empty balances");
            Vec::new()
        }
    };

    let first_seen = match (row.first_seen_slot, row.first_seen_timestamp) {
        (Some(slot), Some(ts)) => Some(SeenAtResponse {
            block_height: slot as u64,
            timestamp: ts.to_rfc3339_opts(SecondsFormat::Millis, true),
        }),
        _ => None,
    };
    let last_seen = match (row.last_seen_slot, row.last_seen_timestamp) {
        (Some(slot), Some(ts)) => Some(SeenAtResponse {
            block_height: slot as u64,
            timestamp: ts.to_rfc3339_opts(SecondsFormat::Millis, true),
        }),
        _ => None,
    };

    Json(AddressSummaryResponse {
        address: addr,
        balances,
        tx_count: (row.txs_sent_count + row.txs_received_count) as u64,
        first_seen,
        last_seen,
        schemas_owned_count: row.schemas_owned_count as u32,
        attestor_member_count: row.attestor_member_count as u32,
    })
    .into_response()
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
