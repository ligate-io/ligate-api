//! Read-side sqlx queries for the v1 indexer endpoints.
//!
//! The indexer task (`ligate-api-indexer`) writes to the same tables
//! these functions read from. Splitting reads and writes across two
//! modules keeps the responsibilities clear — the api crate owns the
//! response-shape mapping, the indexer crate owns the ingest pipeline.
//!
//! All queries return Postgres-shaped types (string hashes,
//! `chrono::DateTime<Utc>` timestamps, raw `i64` heights). The
//! handler layer converts to the wire-format types in
//! [`crate::responses`] before serialising — that's where RFC 0002's
//! "RFC3339 with milliseconds", "u128 as decimal string", etc. live.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

/// One row of the `slots` table, mapped to a Rust shape. Mirrors the
/// table definition in `migrations/20260507000001_init.sql`. The
/// handler layer converts this to [`crate::responses::BlockResponse`].
#[derive(Debug)]
pub struct SlotRow {
    pub height: i64,
    pub hash: String,
    pub prev_hash: Option<String>,
    pub state_root: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub batch_count: i32,
    pub tx_count: i32,
}

/// Read the highest slot height the indexer has written. `None` for
/// fresh boots that have ingested nothing yet.
pub async fn max_slot_height(pool: &PgPool) -> sqlx::Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as("SELECT MAX(height) FROM slots")
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|(h,)| h))
}

/// Read one slot by its height. `None` when the row doesn't exist
/// yet (indexer hasn't caught up to that height, or the height is
/// above the chain's head).
pub async fn slot_by_height(pool: &PgPool, height: i64) -> sqlx::Result<Option<SlotRow>> {
    let row = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<String>,
            Option<String>,
            DateTime<Utc>,
            i32,
            i32,
        ),
    >(
        "SELECT height, hash, prev_hash, state_root, timestamp, batch_count, tx_count
         FROM slots WHERE height = $1",
    )
    .bind(height)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(height, hash, prev_hash, state_root, timestamp, batch_count, tx_count)| SlotRow {
            height,
            hash,
            prev_hash,
            state_root,
            timestamp,
            batch_count,
            tx_count,
        },
    ))
}

/// Read a page of slots, descending by height. `before_height` is the
/// cursor; `None` starts at the head. Fetches `limit + 1` rows so the
/// caller can tell whether a `next` cursor is warranted.
///
/// The `limit + 1` trick avoids a separate `COUNT(*)` or `HAS_MORE`
/// query: if we asked for 20 rows and got 21, there's at least one
/// more page; the 21st row tells us its height (the next page's
/// starting cursor).
pub async fn slots_page(
    pool: &PgPool,
    before_height: Option<i64>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<SlotRow>> {
    // Two query variants instead of one parameterised `$1::bigint`
    // pseudo-NULL because Postgres treats `height < NULL` as
    // unknown (not true), which silently filters out every row.
    // Splitting keeps the planner honest and the SQL readable.
    let rows = match before_height {
        Some(h) => {
            sqlx::query_as::<
                _,
                (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                    DateTime<Utc>,
                    i32,
                    i32,
                ),
            >(
                "SELECT height, hash, prev_hash, state_root, timestamp, batch_count, tx_count
                 FROM slots
                 WHERE height < $1
                 ORDER BY height DESC
                 LIMIT $2",
            )
            .bind(h)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<
                _,
                (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                    DateTime<Utc>,
                    i32,
                    i32,
                ),
            >(
                "SELECT height, hash, prev_hash, state_root, timestamp, batch_count, tx_count
                 FROM slots
                 ORDER BY height DESC
                 LIMIT $1",
            )
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows
        .into_iter()
        .map(
            |(height, hash, prev_hash, state_root, timestamp, batch_count, tx_count)| SlotRow {
                height,
                hash,
                prev_hash,
                state_root,
                timestamp,
                batch_count,
                tx_count,
            },
        )
        .collect())
}

// ---- transactions ----------------------------------------------------------
//
// The `transactions` table is the indexer ingest target — see
// `crates/indexer/src/db.rs::insert_transaction`. These reads join
// against `slots` so a single query returns the block-side fields
// (`block_hash`, `block_timestamp`) the wire shape needs without a
// follow-up roundtrip.

/// One row of the `transactions ⨝ slots` join, mapped to a Rust
/// shape. The handler converts this to [`crate::responses::TxResponse`].
#[derive(Debug)]
pub struct TxRow {
    pub hash: String,
    pub slot: i64,
    pub position: i32,
    pub sender: Option<String>,
    pub sender_pubkey: Option<String>,
    pub nonce: Option<i64>,
    /// Postgres `NUMERIC(78,0)` exposed as `String` via `bigdecimal`.
    /// RFC 0002 wants decimal-string at the wire boundary, so we
    /// surface it as `String` here rather than parsing through a
    /// numeric type that loses precision.
    pub fee_paid_nano: Option<String>,
    pub kind: String,
    pub details: Value,
    pub outcome: String,
    pub revert_reason: Option<String>,
    pub block_hash: Option<String>,
    pub block_timestamp: Option<DateTime<Utc>>,
}

/// Read one tx by hash. `None` if the indexer hasn't written that
/// hash yet — either it's pre-finality on the chain or the tx
/// doesn't exist.
pub async fn tx_by_hash(pool: &PgPool, hash: &str) -> sqlx::Result<Option<TxRow>> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            i64,
            i32,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<sqlx::types::BigDecimal>,
            String,
            Value,
            String,
            Option<String>,
            Option<String>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                t.fee_paid_nano, t.kind, t.details, t.outcome, t.revert_reason,
                s.hash, s.timestamp
         FROM transactions t
         JOIN slots s ON s.height = t.slot
         WHERE t.hash = $1",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(tx_row_from_tuple))
}

/// Cursor shape for `/v1/txs`. Compound (slot desc, position desc)
/// to give a strict-decreasing key for a stable order across reads,
/// even when the indexer is inserting concurrently.
pub struct TxsCursor {
    pub slot: i64,
    pub position: i32,
}

/// Read a page of txs, descending by (slot, position). `before` is
/// the cursor; `None` starts at the head. Fetches `limit + 1` rows
/// for has-more detection (same trick as `slots_page`).
pub async fn txs_page(
    pool: &PgPool,
    before: Option<TxsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<TxRow>> {
    let rows = match before {
        Some(c) => {
            sqlx::query_as::<
                _,
                (
                    String,
                    i64,
                    i32,
                    Option<String>,
                    Option<String>,
                    Option<i64>,
                    Option<sqlx::types::BigDecimal>,
                    String,
                    Value,
                    String,
                    Option<String>,
                    Option<String>,
                    Option<DateTime<Utc>>,
                ),
            >(
                "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                        t.fee_paid_nano, t.kind, t.details, t.outcome, t.revert_reason,
                        s.hash, s.timestamp
                 FROM transactions t
                 JOIN slots s ON s.height = t.slot
                 WHERE (t.slot, t.position) < ($1, $2)
                 ORDER BY t.slot DESC, t.position DESC
                 LIMIT $3",
            )
            .bind(c.slot)
            .bind(c.position)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<
                _,
                (
                    String,
                    i64,
                    i32,
                    Option<String>,
                    Option<String>,
                    Option<i64>,
                    Option<sqlx::types::BigDecimal>,
                    String,
                    Value,
                    String,
                    Option<String>,
                    Option<String>,
                    Option<DateTime<Utc>>,
                ),
            >(
                "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                        t.fee_paid_nano, t.kind, t.details, t.outcome, t.revert_reason,
                        s.hash, s.timestamp
                 FROM transactions t
                 JOIN slots s ON s.height = t.slot
                 ORDER BY t.slot DESC, t.position DESC
                 LIMIT $1",
            )
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.into_iter().map(tx_row_from_tuple).collect())
}

// ---- address_summaries -----------------------------------------------------

/// One row of `address_summaries`, mapped to a Rust shape. The
/// handler converts this to [`crate::responses::AddressSummaryResponse`]
/// after augmenting with chain-side balances.
#[derive(Debug)]
pub struct AddressSummaryRow {
    pub txs_sent_count: i64,
    pub txs_received_count: i64,
    pub first_seen_slot: Option<i64>,
    pub first_seen_timestamp: Option<DateTime<Utc>>,
    pub last_seen_slot: Option<i64>,
    pub last_seen_timestamp: Option<DateTime<Utc>>,
    pub schemas_owned_count: i32,
    pub attestor_member_count: i32,
}

/// Read the summary row for one address. Returns a zeroed-out row
/// (not `None`) when the address has no observed activity — partners
/// asking about a fresh address get a coherent shape with zeros
/// rather than a 404.
pub async fn address_summary(pool: &PgPool, address: &str) -> sqlx::Result<AddressSummaryRow> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        i64,
        i64,
        Option<i64>,
        Option<DateTime<Utc>>,
        Option<i64>,
        Option<DateTime<Utc>>,
        i32,
        i32,
    )> = sqlx::query_as(
        "SELECT txs_sent_count, txs_received_count,
                first_seen_slot, first_seen_timestamp,
                last_seen_slot,  last_seen_timestamp,
                schemas_owned_count, attestor_member_count
         FROM address_summaries
         WHERE address = $1",
    )
    .bind(address)
    .fetch_optional(pool)
    .await?;

    Ok(row
        .map(|t| AddressSummaryRow {
            txs_sent_count: t.0,
            txs_received_count: t.1,
            first_seen_slot: t.2,
            first_seen_timestamp: t.3,
            last_seen_slot: t.4,
            last_seen_timestamp: t.5,
            schemas_owned_count: t.6,
            attestor_member_count: t.7,
        })
        .unwrap_or_else(|| AddressSummaryRow {
            txs_sent_count: 0,
            txs_received_count: 0,
            first_seen_slot: None,
            first_seen_timestamp: None,
            last_seen_slot: None,
            last_seen_timestamp: None,
            schemas_owned_count: 0,
            attestor_member_count: 0,
        }))
}

#[allow(clippy::type_complexity)]
fn tx_row_from_tuple(
    t: (
        String,
        i64,
        i32,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<sqlx::types::BigDecimal>,
        String,
        Value,
        String,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    ),
) -> TxRow {
    let (
        hash,
        slot,
        position,
        sender,
        sender_pubkey,
        nonce,
        fee_paid_nano,
        kind,
        details,
        outcome,
        revert_reason,
        block_hash,
        block_timestamp,
    ) = t;
    TxRow {
        hash,
        slot,
        position,
        sender,
        sender_pubkey,
        nonce,
        // BigDecimal → String. Trimmed of trailing decimal noise so
        // a `1000000000` row comes back as `"1000000000"`, not
        // `"1000000000.0"` (BigDecimal's default Display).
        fee_paid_nano: fee_paid_nano.map(|bd| bd.with_scale(0).to_string()),
        kind,
        details,
        outcome,
        revert_reason,
        block_hash,
        block_timestamp,
    }
}
