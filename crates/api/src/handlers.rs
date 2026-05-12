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
    AddressSummaryResponse, AttestorSetResponse, BlockResponse, InfoResponse, Page, Pagination,
    RegisteredAtResponse, SchemaResponse, SeenAtResponse, TxResponse,
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

/// Cursor shape for `/v1/schemas` (compound: descending by
/// (registered_at_slot, id)). RFC 0001 documents `{"id": "lsc1..."}`
/// as the schemas cursor; the compound form is a strict superset
/// — the wire shape just carries the slot tiebreaker too.
#[derive(Debug, Serialize, Deserialize)]
struct SchemasCursor {
    slot: u64,
    id: String,
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

/// Per-address drip-status response, returned when
/// `GET /v1/drip/status?address=<addr>` carries an `address` query
/// param. `next_drip_at` is RFC3339 UTC millis when `can_drip` is
/// `false`, and `null` when `can_drip` is `true`.
#[derive(Debug, Serialize)]
pub struct AddressDripStatusResponse {
    pub can_drip: bool,
    pub next_drip_at: Option<String>,
}

/// Untagged: the wire body is one of the two shapes inline, with no
/// `kind` discriminator. The explorer's faucet UI peeks
/// `can_drip` to decide; operator dashboards continue reading the
/// global config shape via the no-param call.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum DripStatusBody {
    Global(DripStatusResponse),
    PerAddress(AddressDripStatusResponse),
}

/// Optional `?address=<addr>` query param on `/v1/drip/status`. Empty
/// or absent → global config shape; present → per-address eligibility.
#[derive(Debug, Deserialize)]
pub struct DripStatusQuery {
    pub address: Option<String>,
}

/// `GET /v1/drip/status[?address=<addr>]`.
///
/// Two response shapes, dispatched on the presence of the `address`
/// query param:
///
/// - **No param** (operator dashboards, default explorer headers):
///   global config (drip amount, rate-limit window, distinct
///   addresses dripped, faucet address).
/// - **`?address=lig1...`** (explorer faucet UI): per-address
///   eligibility, `{ can_drip, next_drip_at }`. `next_drip_at` is
///   the absolute RFC3339-millis UTC instant the cooldown clears, or
///   `null` when the address can drip right now.
///
/// The per-address branch reads via the rate-limiter's non-mutating
/// `peek` so it never burns a window slot. Repeated polls from the
/// explorer's faucet page don't accidentally start a cooldown for an
/// address that hasn't actually dripped.
pub async fn drip_status(
    State(state): State<AppState>,
    Query(params): Query<DripStatusQuery>,
) -> impl IntoResponse {
    if let Some(addr) = params.address.as_deref().filter(|s| !s.is_empty()) {
        let body = match state.rate_limiter.peek(addr) {
            RateCheck::Allowed => AddressDripStatusResponse {
                can_drip: true,
                next_drip_at: None,
            },
            RateCheck::Blocked { retry_after } => {
                // `chrono::Utc::now() + retry_after` puts the cooldown
                // boundary in absolute time so the explorer can render
                // a stable "comes back at HH:MM" without re-syncing
                // its clock against the server. Millisecond precision
                // matches the rest of the api's RFC3339 emissions.
                //
                // `from_std` only fails when the `std::time::Duration`
                // exceeds `i64::MAX` milliseconds (~292 million years);
                // a rate-limit window can never reach that. Saturate
                // to `Duration::zero()` on the impossible-fail branch
                // so the handler can't panic on a poisoned input.
                let bump = chrono::Duration::from_std(retry_after)
                    .unwrap_or_else(|_| chrono::Duration::zero());
                let next = chrono::Utc::now() + bump;
                AddressDripStatusResponse {
                    can_drip: false,
                    next_drip_at: Some(next.to_rfc3339_opts(SecondsFormat::Millis, true)),
                }
            }
        };
        return Json(DripStatusBody::PerAddress(body));
    }

    let drip_nano = state.config.drip_amount;
    Json(DripStatusBody::Global(DripStatusResponse {
        drip_amount_nano: drip_nano,
        drip_amount_lgt: nano_to_lgt(drip_nano),
        rate_limit_secs: state.config.drip_rate_limit_secs,
        addresses_dripped: state.rate_limiter.drip_count(),
        faucet_address: state.signer.address(),
    }))
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

/// `GET /v1/schemas` — descending list of registered schemas with
/// compound cursor pagination.
///
/// Reads from `schemas`, ordered by `(registered_at_slot DESC, id
/// DESC)`. Cursor encodes the last row's `(slot, id)`.
pub async fn schemas_list(
    State(state): State<AppState>,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let limit = cursor::resolve_limit(params.limit);
    let before = params
        .before
        .as_deref()
        .and_then(cursor::decode::<SchemasCursor>)
        .map(|c| queries::SchemasCursor {
            registered_at_slot: c.slot as i64,
            id: c.id,
        });

    let limit_plus_one = (limit as i64) + 1;
    let mut rows = match queries::schemas_page(&state.pg, before, limit_plus_one).await {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!(error = %e, "schemas_page in /v1/schemas");
            return internal_error();
        }
    };

    let has_more = rows.len() as i64 > limit as i64;
    if has_more {
        rows.truncate(limit as usize);
    }

    let next = if has_more {
        rows.last().and_then(|r| {
            cursor::encode(&SchemasCursor {
                slot: r.registered_at_slot as u64,
                id: r.id.clone(),
            })
            .ok()
        })
    } else {
        None
    };

    let data: Vec<SchemaResponse> = rows.into_iter().map(schema_row_to_response).collect();

    Json(Page {
        data,
        pagination: Pagination { next, limit },
    })
    .into_response()
}

/// `GET /v1/schemas/{id}` — a single schema by bech32m `lsc1...` id.
pub async fn schema_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row = match queries::schema_by_id(&state.pg, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "schema not found",
                    "tracking": null
                })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, %id, "schema_by_id");
            return internal_error();
        }
    };

    Json(schema_row_to_response(row)).into_response()
}

/// `GET /v1/attestor-sets/{id}` — a single attestor set by bech32m
/// `las1...` id.
pub async fn attestor_set_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row = match queries::attestor_set_by_id(&state.pg, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "attestor set not found",
                    "tracking": null
                })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, %id, "attestor_set_by_id");
            return internal_error();
        }
    };

    // members is JSONB array of strings; pull out the typed Vec for
    // the wire shape. Defensive against malformed rows: drop the
    // payload to an empty list rather than crash.
    let members: Vec<String> = match row.members {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    Json(AttestorSetResponse {
        id: row.id,
        members,
        threshold: row.threshold as u8,
        registered_at: RegisteredAtResponse {
            block_height: row.registered_at_slot as u64,
            tx_hash: row.registered_at_tx,
            timestamp: row
                .registered_at_timestamp
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        },
        schema_count: row.schema_count as u32,
    })
    .into_response()
}

/// Map a `schemas` row to the RFC 0002 wire shape.
fn schema_row_to_response(row: queries::SchemaRow) -> SchemaResponse {
    SchemaResponse {
        id: row.id,
        name: row.name,
        version: row.version as u32,
        owner: row.owner,
        attestor_set_id: row.attestor_set_id,
        fee_routing_bps: row.fee_routing_bps as u16,
        fee_routing_addr: row.fee_routing_addr,
        payload_shape_hash: row.payload_shape_hash,
        registered_at: RegisteredAtResponse {
            block_height: row.registered_at_slot as u64,
            tx_hash: row.registered_at_tx,
            timestamp: row
                .registered_at_timestamp
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        },
        attestation_count: row.attestation_count as u32,
    }
}

// ---- helpers ---------------------------------------------------------------

fn nano_to_lgt(nano: u128) -> f64 {
    (nano as f64) / 1_000_000_000.0
}
