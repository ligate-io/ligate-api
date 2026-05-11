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
