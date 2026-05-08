//! Postgres-side helpers for the indexer.
//!
//! Owns the connection pool, runs migrations on startup, and provides
//! typed insert / upsert helpers for the two v0 tables (`slots` and
//! `indexer_state`). Higher-level loop logic (backfill, live tail)
//! lives in [`crate::ingest`] and reads / writes through these
//! helpers without touching SQL directly.

use chrono::{DateTime, TimeZone, Utc};
use ligate_api_types::{RollupInfo, SlotResponse};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::error::Result;

/// State key under which the latest backfilled slot height is stored.
/// Used as the resume-point cursor on indexer restart.
pub const KEY_LAST_INDEXED_HEIGHT: &str = "last_indexed_height";
pub const KEY_CHAIN_ID: &str = "chain_id";
pub const KEY_CHAIN_HASH: &str = "chain_hash";
pub const KEY_NODE_VERSION: &str = "node_version";

/// Connect to Postgres. Migrations live at the workspace root and are
/// run once by the api crate's main binary at startup; this helper
/// just opens a pool against an already-migrated database. (When the
/// indexer was a standalone binary at `ligate-explorer/crates/indexer/`
/// it ran migrations itself; that responsibility moved to the api crate
/// when both services were unified into a single Postgres in this
/// workspace.)
pub async fn connect(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Bootstrap chain identity into `indexer_state` from a `/v1/rollup/info`
/// response. Called once per indexer startup.
///
/// Idempotent: if the keys already exist with the same values, this
/// is a no-op. If they exist with different values (i.e. operator
/// pointed indexer at a different chain), values are overwritten and
/// the previous chain's slot history is left in place. Operator is
/// expected to truncate `slots` manually if they want a clean re-index.
pub async fn write_chain_identity(pool: &PgPool, info: &RollupInfo) -> Result<()> {
    let mut tx = pool.begin().await?;
    upsert_state(&mut *tx, KEY_CHAIN_ID, &info.chain_id).await?;
    upsert_state(&mut *tx, KEY_CHAIN_HASH, &info.chain_hash).await?;
    upsert_state(&mut *tx, KEY_NODE_VERSION, &info.version).await?;
    tx.commit().await?;
    Ok(())
}

/// Upsert one slot row. Idempotent on (height) primary key, so
/// re-runs of backfill don't create duplicates.
pub async fn upsert_slot(pool: &PgPool, slot: &SlotResponse) -> Result<()> {
    let timestamp = slot
        .timestamp
        .and_then(|s| Utc.timestamp_opt(s as i64, 0).single())
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let raw = serde_json::to_value(slot).unwrap_or(serde_json::Value::Null);

    sqlx::query(
        "INSERT INTO slots (height, hash, prev_hash, state_root, timestamp, batch_count, tx_count, raw)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (height) DO UPDATE SET
            hash        = EXCLUDED.hash,
            prev_hash   = EXCLUDED.prev_hash,
            state_root  = EXCLUDED.state_root,
            timestamp   = EXCLUDED.timestamp,
            batch_count = EXCLUDED.batch_count,
            tx_count    = EXCLUDED.tx_count,
            raw         = EXCLUDED.raw,
            indexed_at  = NOW()",
    )
    .bind(slot.number as i64)
    .bind(&slot.hash)
    .bind(slot.prev_hash.as_deref())
    .bind(slot.state_root.as_deref())
    .bind(timestamp)
    .bind(slot.batch_count.unwrap_or(0) as i32)
    .bind(slot.tx_count.unwrap_or(0) as i32)
    .bind(raw)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the resume-cursor: highest slot height we've already indexed.
/// Returns `None` on a fresh database.
pub async fn read_last_indexed_height(pool: &PgPool) -> Result<Option<u64>> {
    let row = sqlx::query("SELECT v FROM indexer_state WHERE k = $1")
        .bind(KEY_LAST_INDEXED_HEIGHT)
        .fetch_optional(pool)
        .await?;
    Ok(row
        .and_then(|r| r.try_get::<String, _>("v").ok())
        .and_then(|s| s.parse().ok()))
}

/// Write the resume-cursor.
pub async fn write_last_indexed_height(pool: &PgPool, height: u64) -> Result<()> {
    upsert_state(pool, KEY_LAST_INDEXED_HEIGHT, &height.to_string()).await
}

/// Upsert one k/v entry. Used by the helpers above.
async fn upsert_state<'e, E>(executor: E, key: &str, value: &str) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO indexer_state (k, v) VALUES ($1, $2)
         ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, updated_at = NOW()",
    )
    .bind(key)
    .bind(value)
    .execute(executor)
    .await?;
    Ok(())
}

/// Helper for tests: fetch one slot by height. Kept around even
/// without an in-tree consumer because integration-test callers will
/// pull it in via `pub use` once a Postgres-backed test harness
/// lands.
#[cfg(test)]
#[allow(dead_code)]
pub async fn read_slot(pool: &PgPool, height: u64) -> Result<Option<(String, i32)>> {
    let row = sqlx::query("SELECT hash, tx_count FROM slots WHERE height = $1")
        .bind(height as i64)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| {
        let hash: String = r.try_get("hash").ok()?;
        let tx_count: i32 = r.try_get("tx_count").ok()?;
        Some((hash, tx_count))
    }))
}

// `chrono::TimeZone` is unused at compile time without `Utc.timestamp_opt`
// being called, but rustc's unused-import warning fires anyway under
// `-D warnings`. The use above ensures it stays referenced.
#[allow(dead_code)]
fn _ensure_chrono_referenced() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}
