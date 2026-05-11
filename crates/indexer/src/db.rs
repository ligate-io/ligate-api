//! Postgres-side helpers for the indexer.
//!
//! Owns the connection pool, runs migrations on startup, and provides
//! typed insert / upsert helpers for the two v0 tables (`slots` and
//! `indexer_state`). Higher-level loop logic (backfill, live tail)
//! lives in [`crate::ingest`] and reads / writes through these
//! helpers without touching SQL directly.

use chrono::{DateTime, TimeZone, Utc};
use ligate_api_types::{RollupInfo, SlotResponse};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::error::Result;
use crate::parser::{ClassifiedTx, IndexerTx, TxOutcome};

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

/// Insert one parsed transaction into the `transactions` table.
///
/// Idempotent on the `hash` primary key — re-running backfill against
/// the same slots doesn't create duplicates. The `details` JSONB shape
/// is per-`kind` per RFC 0002. Sender / nonce / fee fields are nullable
/// because the chain elides the borsh-encoded body from REST (see
/// migration 0003); the indexer records what it can derive from
/// emitted events and leaves the rest `NULL`.
///
/// `raw` captures the full event payloads + tx number / batch number,
/// so deep-dive views can extract fields the typed columns don't yet
/// model.
pub async fn insert_transaction(
    pool: &PgPool,
    classified: &ClassifiedTx,
    slot_height: u64,
    position_in_block: i32,
    raw_event_keys: &[String],
) -> Result<()> {
    // Map the parser's IndexerTx variant to (kind_string, details_json).
    let (kind, details) = match &classified.kind {
        IndexerTx::Transfer(t) => (
            "transfer",
            serde_json::json!({
                "from": t.from,
                "to": t.to,
                "amount_nano": t.amount_nano,
                "token_id": t.token_id,
            }),
        ),
        IndexerTx::Unknown { event_keys } => (
            "unknown",
            // RFC 0002 reserves `raw_call_disc: [u8, u8]` for the
            // typed-but-unknown discriminator pair; the parser
            // doesn't have access to the raw body bytes today (chain
            // elides them), so we surface event keys instead as the
            // forensic field. Schema's `details` is JSONB so adding
            // a `raw_call_disc` field later is non-breaking.
            serde_json::json!({ "event_keys": event_keys }),
        ),
    };

    // Outcome: parser already filtered out `Skipped`, so we only see
    // `Committed` or `Reverted` here. Map to the SQL CHECK constraint
    // wording.
    let outcome = match classified.outcome {
        TxOutcome::Committed => "committed",
        TxOutcome::Reverted => "reverted",
        TxOutcome::Skipped => {
            // Shouldn't happen — classify_tx returns None for skipped
            // — but be defensive rather than panic.
            return Ok(());
        }
    };

    // Capture forensic data so the `raw` column has something useful
    // for explorer deep-dive views. The schema requires it NOT NULL.
    let raw: Value = serde_json::json!({
        "batch_number": classified.batch_number,
        "global_tx_number": classified.global_tx_number,
        "event_keys": raw_event_keys,
    });

    // Per RFC 0002 / migration 0003, sender / sender_pubkey / nonce /
    // fee_paid_nano are nullable. For Transfer txs we can fill `sender`
    // from the event payload's `from.user`; pubkey / nonce / fee remain
    // null until the chain exposes them on the REST surface.
    let sender: Option<&str> = match &classified.kind {
        IndexerTx::Transfer(t) => Some(t.from.as_str()),
        IndexerTx::Unknown { .. } => None,
    };

    sqlx::query(
        "INSERT INTO transactions (
            hash, slot, position, sender, sender_pubkey, nonce, fee_paid_nano,
            kind, details, raw, outcome, revert_reason
         ) VALUES (
            $1, $2, $3, $4, NULL, NULL, NULL,
            $5, $6, $7, $8, NULL
         )
         ON CONFLICT (hash) DO UPDATE SET
            slot         = EXCLUDED.slot,
            position     = EXCLUDED.position,
            sender       = EXCLUDED.sender,
            kind         = EXCLUDED.kind,
            details      = EXCLUDED.details,
            raw          = EXCLUDED.raw,
            outcome      = EXCLUDED.outcome,
            indexed_at   = NOW()",
    )
    .bind(&classified.hash)
    .bind(slot_height as i64)
    .bind(position_in_block)
    .bind(sender)
    .bind(kind)
    .bind(details)
    .bind(raw)
    .bind(outcome)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update the `address_summaries` row for one address role in one tx.
///
/// Roles:
///
/// - `AddressRole::Sender` increments `txs_sent_count`. Called for
///   every tx insert where `sender` is non-null.
/// - `AddressRole::Receiver` increments `txs_received_count`. Called
///   for `IndexerTx::Transfer` where `details.to` is the address.
///
/// `first_seen` is set on the first observation; `last_seen` updates
/// monotonically when (slot, tx_hash) is more recent than the
/// existing value. Concurrent-ingest-safe via `ON CONFLICT DO UPDATE`
/// with greatest-wins semantics for `last_seen`.
///
/// `schemas_owned_count` and `attestor_member_count` are left at
/// their existing values here; those counters are maintained by the
/// schema / attestor-set ingest paths (Phase D), which haven't landed
/// yet because the chain doesn't emit typed events for them (see
/// ligate-chain#295). Reads always succeed; the count fields just
/// stay 0 until Phase D wires their increments.
pub async fn upsert_address_activity(
    pool: &PgPool,
    address: &str,
    role: AddressRole,
    slot_height: u64,
    tx_hash: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let (sent_inc, recv_inc) = match role {
        AddressRole::Sender => (1, 0),
        AddressRole::Receiver => (0, 1),
    };

    sqlx::query(
        "INSERT INTO address_summaries (
            address,
            txs_sent_count, txs_received_count,
            first_seen_slot, first_seen_tx, first_seen_timestamp,
            last_seen_slot,  last_seen_tx,  last_seen_timestamp
         ) VALUES (
            $1,
            $2, $3,
            $4, $5, $6,
            $4, $5, $6
         )
         ON CONFLICT (address) DO UPDATE SET
            txs_sent_count       = address_summaries.txs_sent_count + EXCLUDED.txs_sent_count,
            txs_received_count   = address_summaries.txs_received_count + EXCLUDED.txs_received_count,
            -- first_seen is sticky: keep whichever was earliest.
            first_seen_slot      = LEAST(address_summaries.first_seen_slot, EXCLUDED.first_seen_slot),
            first_seen_tx        = CASE
                                     WHEN address_summaries.first_seen_slot IS NULL
                                       OR EXCLUDED.first_seen_slot < address_summaries.first_seen_slot
                                     THEN EXCLUDED.first_seen_tx
                                     ELSE address_summaries.first_seen_tx
                                   END,
            first_seen_timestamp = CASE
                                     WHEN address_summaries.first_seen_timestamp IS NULL
                                       OR EXCLUDED.first_seen_timestamp < address_summaries.first_seen_timestamp
                                     THEN EXCLUDED.first_seen_timestamp
                                     ELSE address_summaries.first_seen_timestamp
                                   END,
            -- last_seen advances: keep whichever was later.
            last_seen_slot       = GREATEST(address_summaries.last_seen_slot, EXCLUDED.last_seen_slot),
            last_seen_tx         = CASE
                                     WHEN address_summaries.last_seen_slot IS NULL
                                       OR EXCLUDED.last_seen_slot > address_summaries.last_seen_slot
                                     THEN EXCLUDED.last_seen_tx
                                     ELSE address_summaries.last_seen_tx
                                   END,
            last_seen_timestamp  = CASE
                                     WHEN address_summaries.last_seen_timestamp IS NULL
                                       OR EXCLUDED.last_seen_timestamp > address_summaries.last_seen_timestamp
                                     THEN EXCLUDED.last_seen_timestamp
                                     ELSE address_summaries.last_seen_timestamp
                                   END,
            indexed_at           = NOW()",
    )
    .bind(address)
    .bind(sent_inc)
    .bind(recv_inc)
    .bind(slot_height as i64)
    .bind(tx_hash)
    .bind(timestamp)
    .execute(pool)
    .await?;
    Ok(())
}

/// Which side of the tx the address played. Drives which counter
/// gets incremented in [`upsert_address_activity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressRole {
    Sender,
    Receiver,
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
