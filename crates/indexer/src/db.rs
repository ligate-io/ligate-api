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
use crate::parser::{
    ClassifiedTx, IndexerRegisterAttestorSet, IndexerRegisterSchema, IndexerSubmitAttestation,
    IndexerTx, TxOutcome,
};

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
///
/// `proposer` is the Celestia `da_address` of the sequencer that
/// submitted the slot's first batch to DA. The caller (ingest loop)
/// extracts it from `batch.receipt.da_address`. `None` if the slot
/// has no batches or the batch fetch failed; column stays NULL.
///
/// **prev_hash derivation.** Chain's slot JSON doesn't include a
/// prev_hash field today (verified live across multiple slots on
/// rpc.ligate.io). When `slot.prev_hash` is `None`, this helper
/// derives it via a lookup against slot N-1's stored `hash`. For
/// genesis (`height = 0`) it's `None` by definition. The lookup is
/// a primary-key index hit (`slots.height`) so cost is O(1).
///
/// **finalized_at stamping.** The chain emits `finality_status`
/// as `"pending"` or `"finalized"`. We mirror it onto the row.
/// When a row transitions pending → finalized (observed at upsert
/// or re-poll time), we stamp `finalized_at = NOW()`. This is an
/// observation, not the true chain finalization moment (chain
/// doesn't emit that), but it's within one indexer poll interval
/// of truth — accurate enough for `/v1/stats/finality` percentiles.
pub async fn upsert_slot(pool: &PgPool, slot: &SlotResponse, proposer: Option<&str>) -> Result<()> {
    // Chain emits `slot.timestamp` as Unix MILLISECONDS (verified
    // against localnet: 1778527856952 → 2026-05-11T...). Earlier
    // code parsed via `timestamp_opt(s, 0)` which treats the input
    // as seconds — produces year +58329. `timestamp_millis_opt`
    // is the right routing.
    let timestamp = slot
        .timestamp
        .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let raw = serde_json::to_value(slot).unwrap_or(serde_json::Value::Null);

    // prev_hash: prefer the chain-provided value (forward-compat for
    // when chain starts emitting the field), otherwise look up slot
    // N-1's hash. None for genesis (height 0) and for slots whose
    // predecessor isn't yet indexed (which shouldn't normally happen
    // because we ingest sequentially, but defensible against
    // restart-mid-backfill).
    let prev_hash: Option<String> = match slot.prev_hash.as_deref() {
        Some(h) => Some(h.to_string()),
        None if slot.number > 0 => {
            sqlx::query_scalar::<_, String>("SELECT hash FROM slots WHERE height = $1")
                .bind((slot.number - 1) as i64)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
        }
        _ => None,
    };

    sqlx::query(
        "INSERT INTO slots (
            height, hash, prev_hash, state_root, timestamp,
            batch_count, tx_count, proposer, finality_status, finalized_at, raw
         )
         VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9,
            -- New rows stamp finalized_at immediately if the chain
            -- already reports the slot as finalized (typical for
            -- backfill of historical slots). For new live slots the
            -- chain reports 'pending' first, so this is NULL and
            -- the re-poll loop will stamp NOW() on the flip.
            CASE WHEN $9 = 'finalized' THEN NOW() ELSE NULL END,
            $10
         )
         ON CONFLICT (height) DO UPDATE SET
            hash             = EXCLUDED.hash,
            -- Only overwrite prev_hash if EXCLUDED has one. Preserves
            -- a previously-derived value if a re-fetch returns NULL.
            prev_hash        = COALESCE(EXCLUDED.prev_hash, slots.prev_hash),
            state_root       = EXCLUDED.state_root,
            timestamp        = EXCLUDED.timestamp,
            batch_count      = EXCLUDED.batch_count,
            tx_count         = EXCLUDED.tx_count,
            -- Same COALESCE pattern for proposer: don't blank out a
            -- known value if a later upsert (e.g. re-poll) doesn't
            -- carry batch data.
            proposer         = COALESCE(EXCLUDED.proposer, slots.proposer),
            finality_status  = EXCLUDED.finality_status,
            -- Stamp finalized_at exactly once, on the transition
            -- from non-'finalized' to 'finalized'. Subsequent
            -- finalized→finalized upserts preserve the original
            -- stamp (don't overwrite with a later NOW()).
            finalized_at     = CASE
                WHEN slots.finality_status IS DISTINCT FROM 'finalized'
                 AND EXCLUDED.finality_status = 'finalized'
                THEN NOW()
                ELSE slots.finalized_at
            END,
            raw              = EXCLUDED.raw,
            indexed_at       = NOW()",
    )
    .bind(slot.number as i64)
    .bind(&slot.hash)
    .bind(prev_hash.as_deref())
    .bind(slot.state_root.as_deref())
    .bind(timestamp)
    .bind(slot.batch_count.unwrap_or(0) as i32)
    .bind(slot.tx_count.unwrap_or(0) as i32)
    .bind(proposer)
    .bind(slot.finality_status.as_deref())
    .bind(raw)
    .execute(pool)
    .await?;
    Ok(())
}

/// Detect and apply pending → finalized transitions for slots
/// already in the DB. Called by the indexer's background re-poll
/// task on a fixed interval (typically 10s). Cheap because:
///
/// - The candidate set is bounded (Mocha finalizes within ~3 slots
///   so `finality_status = 'pending'` holds maybe 5 rows at a time)
/// - The partial index `slots_finality_pending_idx` covers the
///   candidate query directly — no full-table scan
/// - The chain-side fetch is one HTTP per candidate
///
/// Returns the number of rows whose status flipped this pass, for
/// the caller's metrics / debug logging.
///
/// **finalized_at semantics.** When the chain reports a slot as
/// finalized that we previously had as pending, we stamp `NOW()` —
/// not the chain's timestamp. This is the *observation* instant
/// (the moment the indexer noticed the flip), which lags the true
/// finalization instant by up to one poll interval. For
/// `/v1/stats/finality` percentile sampling this is the worst-case
/// 10s of upward bias on the latency distribution, acceptable as
/// the v0 "observed" mode. True finalization time would require
/// chain-side `BlobExecutionStatus` subscription (tracked as a
/// followup; not in this PR).
pub async fn flip_pending_slots(pool: &PgPool, rechecked: &[(u64, Option<&str>)]) -> Result<u64> {
    if rechecked.is_empty() {
        return Ok(0);
    }
    let mut flipped: u64 = 0;
    for (height, status) in rechecked {
        if !matches!(status, Some("finalized")) {
            continue;
        }
        let res = sqlx::query(
            "UPDATE slots
             SET finality_status = 'finalized',
                 finalized_at    = NOW(),
                 indexed_at      = NOW()
             WHERE height = $1
               AND finality_status = 'pending'",
        )
        .bind(*height as i64)
        .execute(pool)
        .await?;
        flipped += res.rows_affected();
    }
    Ok(flipped)
}

/// Read the set of heights currently in `'pending'` state. Used by
/// the re-poll loop to know which slots to refetch on each tick.
/// Index-only scan against `slots_finality_pending_idx`; expected
/// to return a handful of rows under normal operation.
pub async fn pending_slot_heights(pool: &PgPool) -> Result<Vec<u64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT height FROM slots WHERE finality_status = 'pending' ORDER BY height ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(h,)| h as u64).collect())
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
        IndexerTx::RegisterAttestorSet(d) => (
            "register_attestor_set",
            serde_json::json!({
                "attestor_set_id": d.attestor_set_id,
                "members": d.members,
                "threshold": d.threshold,
            }),
        ),
        IndexerTx::RegisterSchema(d) => (
            "register_schema",
            serde_json::json!({
                "schema_id": d.schema_id,
                "name": d.name,
                "version": d.version,
                "attestor_set_id": d.attestor_set_id,
                "fee_routing_bps": d.fee_routing_bps,
                "fee_routing_addr": d.fee_routing_addr,
                "payload_shape_hash": d.payload_shape_hash,
            }),
        ),
        IndexerTx::SubmitAttestation(d) => (
            "submit_attestation",
            serde_json::json!({
                "schema_id": d.schema_id,
                "payload_hash": d.payload_hash,
                "signature_count": d.signature_count,
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
    // from the event payload's `from.user`; for Attestation-module
    // events we get sender from the typed payload (registered_by /
    // owner / submitter); pubkey / nonce / fee remain null until the
    // chain exposes them on the REST surface.
    let sender: Option<&str> = match &classified.kind {
        IndexerTx::Transfer(t) => Some(t.from.as_str()),
        IndexerTx::RegisterAttestorSet(d) => Some(d.registered_by.as_str()),
        IndexerTx::RegisterSchema(d) => Some(d.owner.as_str()),
        IndexerTx::SubmitAttestation(d) => Some(d.submitter.as_str()),
        IndexerTx::Unknown { .. } => None,
    };

    // Protocol fee: flat per-kind constant from devnet-1's
    // `chain/devnet-1/genesis/attestation.json`. For testnet/mainnet
    // these values change per governance; the indexer should ideally
    // extract from the `Bank/TokenTransferred` event(s) the chain
    // emits alongside attestation events so the value is always
    // genesis-config-accurate. Filed as a follow-up; the hardcoded
    // table here matches the live devnet-1 config and the backfill
    // in migration 0005.
    //
    // Bound as `Option<i64>` because all four values fit in i64 and
    // Postgres implicit-casts to NUMERIC(78,0). `None` for `unknown`
    // because we don't have a confident value.
    let protocol_fee_nano: Option<i64> = match &classified.kind {
        IndexerTx::Transfer(_) => Some(0),
        IndexerTx::RegisterAttestorSet(_) => Some(50_000_000),
        IndexerTx::RegisterSchema(_) => Some(100_000_000),
        IndexerTx::SubmitAttestation(_) => Some(100_000),
        IndexerTx::Unknown { .. } => None,
    };

    sqlx::query(
        "INSERT INTO transactions (
            hash, slot, position, sender, sender_pubkey, nonce,
            fee_paid_nano, protocol_fee_nano,
            kind, details, raw, outcome, revert_reason
         ) VALUES (
            $1, $2, $3, $4, NULL, NULL,
            NULL, $5,
            $6, $7, $8, $9, NULL
         )
         ON CONFLICT (hash) DO UPDATE SET
            slot              = EXCLUDED.slot,
            position          = EXCLUDED.position,
            sender            = EXCLUDED.sender,
            protocol_fee_nano = EXCLUDED.protocol_fee_nano,
            kind              = EXCLUDED.kind,
            details           = EXCLUDED.details,
            raw               = EXCLUDED.raw,
            outcome           = EXCLUDED.outcome,
            indexed_at        = NOW()",
    )
    .bind(&classified.hash)
    .bind(slot_height as i64)
    .bind(position_in_block)
    .bind(sender)
    .bind(protocol_fee_nano)
    .bind(kind)
    .bind(details)
    .bind(raw)
    .bind(outcome)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert one row into `attestor_sets` from a parsed
/// `Attestation/AttestorSetRegistered` event payload.
///
/// Idempotent on (id) primary key. Maintains `attestor_sets.members`
/// as a JSONB array of bech32m `lpk1...` strings — same order the
/// chain canonicalises to. `schema_count` starts at 0 and is bumped
/// by [`bump_attestor_set_schema_count`] when a schema registers
/// against this set.
///
/// FK: `registered_at_tx` references `transactions(hash)`. Caller
/// must `insert_transaction` BEFORE calling this; the ingest loop
/// already does so.
pub async fn insert_attestor_set(
    pool: &PgPool,
    d: &IndexerRegisterAttestorSet,
    tx_hash: &str,
    slot_height: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let members_json = serde_json::Value::Array(
        d.members
            .iter()
            .map(|m| serde_json::Value::String(m.clone()))
            .collect(),
    );

    sqlx::query(
        "INSERT INTO attestor_sets (
            id, members, threshold,
            registered_at_slot, registered_at_tx, registered_at_timestamp,
            schema_count
         ) VALUES ($1, $2, $3, $4, $5, $6, 0)
         ON CONFLICT (id) DO UPDATE SET
            members                = EXCLUDED.members,
            threshold              = EXCLUDED.threshold,
            registered_at_slot     = EXCLUDED.registered_at_slot,
            registered_at_tx       = EXCLUDED.registered_at_tx,
            registered_at_timestamp = EXCLUDED.registered_at_timestamp,
            indexed_at             = NOW()",
    )
    .bind(&d.attestor_set_id)
    .bind(members_json)
    .bind(i32::from(d.threshold))
    .bind(slot_height as i64)
    .bind(tx_hash)
    .bind(timestamp)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert one row into `schemas` from a parsed
/// `Attestation/SchemaRegistered` event payload.
///
/// FKs: `attestor_set_id` → `attestor_sets(id)`, `registered_at_tx`
/// → `transactions(hash)`. The chain emits `RegisterSchema` only
/// AFTER the bound `RegisterAttestorSet`, and the ingest loop
/// inserts transactions before resource rows — so both FKs are
/// satisfied in normal operation. If the FK fails (e.g. ingest
/// started mid-stream and missed the attestor set), the insert
/// errors and the caller logs-and-continues.
pub async fn insert_schema(
    pool: &PgPool,
    d: &IndexerRegisterSchema,
    tx_hash: &str,
    slot_height: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO schemas (
            id, name, version, owner, attestor_set_id,
            fee_routing_bps, fee_routing_addr, payload_shape_hash,
            registered_at_slot, registered_at_tx, registered_at_timestamp,
            attestation_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 0
         )
         ON CONFLICT (id) DO UPDATE SET
            -- Schemas are immutable on-chain; the only field that can
            -- change at re-ingest is the bookkeeping. `indexed_at`
            -- bump signals \"saw this row again\".
            indexed_at = NOW()",
    )
    .bind(&d.schema_id)
    .bind(&d.name)
    .bind(i32::try_from(d.version).unwrap_or(i32::MAX))
    .bind(&d.owner)
    .bind(&d.attestor_set_id)
    .bind(i32::from(d.fee_routing_bps))
    .bind(d.fee_routing_addr.as_deref())
    .bind(&d.payload_shape_hash)
    .bind(slot_height as i64)
    .bind(tx_hash)
    .bind(timestamp)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert one row into `attestations`. `submitter_pubkey` is NULL
/// (chain doesn't carry it in the event payload; migration 0004
/// loosened the NOT NULL).
///
/// FKs: `schema_id` → `schemas(id)`, `submitted_at_tx` →
/// `transactions(hash)`.
pub async fn insert_attestation(
    pool: &PgPool,
    d: &IndexerSubmitAttestation,
    tx_hash: &str,
    slot_height: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO attestations (
            schema_id, payload_hash, submitter, submitter_pubkey,
            submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
            signature_count
         ) VALUES ($1, $2, $3, NULL, $4, $5, $6, $7)
         ON CONFLICT (schema_id, payload_hash, submitted_at_tx) DO UPDATE SET
            signature_count = EXCLUDED.signature_count,
            indexed_at      = NOW()",
    )
    .bind(&d.schema_id)
    .bind(&d.payload_hash)
    .bind(&d.submitter)
    .bind(slot_height as i64)
    .bind(tx_hash)
    .bind(timestamp)
    .bind(i32::try_from(d.signature_count).unwrap_or(i32::MAX))
    .execute(pool)
    .await?;
    Ok(())
}

/// Bump `attestor_sets.schema_count` for the given attestor set id.
///
/// Called after `insert_schema` succeeds; tracks how many schemas
/// have bound to each attestor set. Schemas are immutable on-chain
/// so this counter is monotonically increasing; never decremented.
///
/// No-op if the attestor set doesn't exist (FK-violation territory;
/// the schema insert would already have failed).
pub async fn bump_attestor_set_schema_count(pool: &PgPool, attestor_set_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE attestor_sets
         SET schema_count = schema_count + 1, indexed_at = NOW()
         WHERE id = $1",
    )
    .bind(attestor_set_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bump `schemas.attestation_count` for the given schema id.
///
/// Called after `insert_attestation` succeeds. Same monotonicity
/// rules as `bump_attestor_set_schema_count`.
pub async fn bump_schema_attestation_count(pool: &PgPool, schema_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE schemas
         SET attestation_count = attestation_count + 1, indexed_at = NOW()
         WHERE id = $1",
    )
    .bind(schema_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bump `address_summaries.schemas_owned_count` for the schema owner
/// when a schema registers. Upserts the row if the address has no
/// existing summary yet (a fresh owner who's never sent a tx),
/// initialising counters at 0 and seeding first_seen/last_seen with
/// the schema-registration tx so the summary stays internally
/// consistent (the CHECK constraint requires all-or-nothing).
pub async fn bump_address_schemas_owned(
    pool: &PgPool,
    owner: &str,
    slot_height: u64,
    tx_hash: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO address_summaries (
            address,
            txs_sent_count, txs_received_count,
            first_seen_slot, first_seen_tx, first_seen_timestamp,
            last_seen_slot,  last_seen_tx,  last_seen_timestamp,
            schemas_owned_count, attestor_member_count
         ) VALUES (
            $1, 0, 0, $2, $3, $4, $2, $3, $4, 1, 0
         )
         ON CONFLICT (address) DO UPDATE SET
            schemas_owned_count = address_summaries.schemas_owned_count + 1,
            indexed_at          = NOW()",
    )
    .bind(owner)
    .bind(slot_height as i64)
    .bind(tx_hash)
    .bind(timestamp)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bump `address_summaries.attestor_member_count` for one member
/// pubkey when an attestor set registers. The schema's
/// `attestor_member_count` field is on the address summary, not the
/// pubkey directly; the indexer can't trivially resolve pubkey →
/// address without a chain query. For v0 we store the count keyed
/// by the bech32m `lpk1...` pubkey string itself in the
/// `address_summaries.address` column — partners querying by
/// address would not find it. Tracked as a v0 gap; a follow-up can
/// derive `address = pubkey[..28]` per the chain's address rule.
///
/// For now: skip. Calling this is a no-op until address resolution
/// lands. The schema's column stays correct (default 0); we just
/// can't increment it without the address derivation.
pub async fn bump_attestor_member_count(_pool: &PgPool, _member_pubkey: &str) -> Result<()> {
    // No-op for v0. See doc comment.
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
