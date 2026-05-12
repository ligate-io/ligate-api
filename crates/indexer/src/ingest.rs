//! Ingest loop for `ligate-indexer`.
//!
//! The loop has two phases that share state:
//!
//! 1. **Bootstrap.** Fetch `/v1/rollup/info`, write chain identity to
//!    `indexer_state`. Pure side effect; runs once per startup.
//! 2. **Run forever.** Pulls `/v1/ledger/slots/latest` to find the
//!    head, then walks `last_indexed_height + 1 ..= head` and writes
//!    each slot. After catching up, sleeps a beat and re-checks the
//!    head. Restart-safe: the resume cursor is persisted to
//!    `indexer_state` after every successful write.
//!
//! Failures during the loop are logged + retried with bounded backoff
//! rather than terminating the process. The chain may be restarting,
//! the network blipping, or Postgres rebooting; the right behaviour
//! for an indexer is to wait it out, not crash.

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use ligate_api_types::{LedgerEvent, LedgerTx, SlotResponse};
use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::client::NodeClient;
use crate::db::{self, AddressRole};
use crate::error::IndexerError;
use crate::parser::{self, IndexerTx};

/// How long to wait between head-checks once we've caught up.
const TAIL_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long to wait after a transient error before retrying.
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Run the indexer end-to-end. Bootstraps chain identity, then loops
/// forever between catching up to the head and tailing.
pub async fn run(client: NodeClient, pool: PgPool, start_height: Option<u64>) -> ! {
    // Bootstrap. If the node is unreachable on startup, we keep
    // retrying rather than failing fast — the indexer might be coming
    // up before the node in a docker-compose unit.
    loop {
        match client.rollup_info().await {
            Ok(info) => {
                if let Err(e) = db::write_chain_identity(&pool, &info).await {
                    error!(error = %e, "writing chain identity to db");
                    tokio::time::sleep(ERROR_BACKOFF).await;
                    continue;
                }
                info!(
                    chain_id = %info.chain_id,
                    chain_hash = %info.chain_hash,
                    version = %info.version,
                    "chain identity bootstrapped"
                );
                break;
            }
            Err(IndexerError::NodeUnreachable(e)) => {
                warn!(error = %e, "node unreachable on bootstrap; retrying");
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
            Err(e) => {
                error!(error = %e, "fatal bootstrap error; retrying anyway");
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
        }
    }

    // Resolve resume cursor. CLI flag overrides DB value if set.
    let mut next_height: u64 = match start_height {
        Some(h) => {
            info!(start_height = h, "starting at CLI-supplied height");
            h
        }
        None => match db::read_last_indexed_height(&pool).await {
            Ok(Some(h)) => {
                info!(last_indexed = h, "resuming from db-stored cursor");
                h.saturating_add(1)
            }
            Ok(None) => {
                info!("fresh db; starting at slot 1");
                1
            }
            Err(e) => {
                error!(error = %e, "reading cursor; defaulting to 1");
                1
            }
        },
    };

    // Main loop: catch up to head, then tail.
    loop {
        // What's the head?
        let head = match client.latest_slot().await {
            Ok(s) => s.number,
            Err(e) => {
                warn!(error = %e, "fetching head; backing off");
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };

        if next_height > head {
            // Already caught up; tail.
            debug!(head, next = next_height, "at head; tailing");
            tokio::time::sleep(TAIL_POLL_INTERVAL).await;
            continue;
        }

        // Walk forward. `while` (not `for`) so we can advance the
        // cursor inside the loop without tripping clippy's
        // `mut_range_bound` (which fires when a `for` body mutates
        // the range bound, since that has no effect on iteration).
        while next_height <= head {
            let h = next_height;
            match client.slot_at(h).await {
                Ok(Some(slot)) => {
                    if let Err(e) = db::upsert_slot(&pool, &slot).await {
                        error!(error = %e, height = h, "writing slot; will retry");
                        tokio::time::sleep(ERROR_BACKOFF).await;
                        // Don't advance `next_height`; outer loop
                        // will refetch head and try again.
                        break;
                    }
                    // Walk the slot's batches → txs → events and
                    // write parsed transactions. Logs the failure
                    // and keeps going on error: tx ingest is a
                    // best-effort layer on top of slot ingest, and
                    // the slot row itself has already landed, so
                    // the next-height cursor advances. A later
                    // backfill PR can re-walk slots whose tx ingest
                    // failed by comparing tx_count to actual row
                    // counts in the transactions table.
                    if let Err(e) = ingest_slot_transactions(&client, &pool, &slot).await {
                        warn!(error = %e, height = h, "ingesting slot transactions; continuing");
                    }
                    if let Err(e) = db::write_last_indexed_height(&pool, h).await {
                        warn!(error = %e, height = h, "updating cursor (slot was written)");
                    }
                    next_height = h + 1;
                }
                Ok(None) => {
                    // Chain returned 404 for a height we already knew
                    // existed (head was >= h). Skip and continue;
                    // shouldn't happen unless the node is reorging
                    // or restarting from a different snapshot.
                    warn!(
                        height = h,
                        "node returned 404 for known-good height; skipping"
                    );
                    next_height = h + 1;
                }
                Err(e) => {
                    warn!(error = %e, height = h, "fetching slot; backing off");
                    tokio::time::sleep(ERROR_BACKOFF).await;
                    break;
                }
            }
        }
    }
}

/// Walk one slot's batches → txs → events, classify each tx, and
/// write rows to the `transactions` table.
///
/// Error handling: returns an error on chain-fetch failures (caller
/// logs and continues with the next slot). Per-tx classify / db
/// failures are logged but don't abort the slot — a single
/// unparseable tx shouldn't halt ingest for the whole slot.
async fn ingest_slot_transactions(
    client: &NodeClient,
    pool: &PgPool,
    slot: &SlotResponse,
) -> Result<(), IndexerError> {
    let Some(batch_range) = slot.batch_range else {
        // Slot doesn't expose a batch_range — chain rev that doesn't
        // emit it, or a slot with zero batches. Nothing to do.
        return Ok(());
    };

    // Slot timestamp for first_seen / last_seen denormalisation.
    // Chain emits Unix MILLISECONDS in `slot.timestamp`; routing
    // through `timestamp_millis_opt` keeps `address_summaries`
    // first_seen / last_seen in sync with `slots.timestamp`. Fall
    // back to UNIX_EPOCH so the field is never null at the address
    // summary level (the `first_seen` / `last_seen` CHECK constraints
    // are all-or-nothing).
    let slot_timestamp: DateTime<Utc> = slot
        .timestamp
        .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    // Fetch every event for this slot, grouped by `tx_hash`. One
    // network call serves the whole slot's classification, avoiding
    // a per-tx fetch on the events endpoint.
    let all_events: Vec<LedgerEvent> = client.events_for_slot(slot.number).await?;

    // Walk batches, then walk each batch's tx_range. Track
    // position-in-slot for the `transactions.position` column.
    let mut position_in_slot: i32 = 0;
    for batch_number in batch_range.start..batch_range.end {
        let batch = match client.batch_at(batch_number).await? {
            Some(b) => b,
            None => {
                warn!(batch_number, "batch fetch returned 404; skipping");
                continue;
            }
        };

        for tx_number in batch.tx_range.start..batch.tx_range.end {
            let tx: LedgerTx = match client.tx_at_number(tx_number).await? {
                Some(t) => t,
                None => {
                    warn!(tx_number, "tx fetch returned 404; skipping");
                    continue;
                }
            };

            // Group events for this tx by matching tx_hash. The chain
            // emits the same bech32m form on both `LedgerTx.hash` and
            // `LedgerEvent.tx_hash` (since SDK fork rev `49e9b2057`
            // landed via ligate-chain #300), so a straight equality
            // check is enough.
            let tx_events: Vec<&LedgerEvent> = all_events
                .iter()
                .filter(|e| e.tx_hash == tx.hash)
                .collect();

            let raw_event_keys: Vec<String> = tx_events.iter().map(|e| e.key.clone()).collect();

            if let Some(classified) = parser::classify_tx(&tx, &tx_events) {
                if let Err(e) = db::insert_transaction(
                    pool,
                    &classified,
                    slot.number,
                    position_in_slot,
                    &raw_event_keys,
                )
                .await
                {
                    warn!(
                        error = %e,
                        tx_hash = %classified.hash,
                        slot = slot.number,
                        "inserting tx; continuing"
                    );
                } else {
                    // Tx row landed. Fan out to the per-kind resource
                    // inserts (attestor_sets / schemas / attestations)
                    // and the denormalised counter maintenance. Each
                    // is independent — log and continue on failure
                    // rather than back the tx out.
                    if let Err(e) =
                        update_address_summaries(pool, &classified, slot.number, slot_timestamp)
                            .await
                    {
                        warn!(
                            error = %e,
                            tx_hash = %classified.hash,
                            slot = slot.number,
                            "updating address_summaries; counter stale"
                        );
                    }
                    if let Err(e) =
                        insert_resource_rows(pool, &classified, slot.number, slot_timestamp).await
                    {
                        warn!(
                            error = %e,
                            tx_hash = %classified.hash,
                            slot = slot.number,
                            "inserting resource rows (attestor_set/schema/attestation); continuing"
                        );
                    }
                }
                position_in_slot += 1;
            }
            // Skipped txs (classify_tx returned None) don't get a
            // row and don't increment the position counter — they
            // didn't land in chain state.
        }
    }

    debug!(
        slot = slot.number,
        batches = batch_range.end - batch_range.start,
        txs_inserted = position_in_slot,
        "slot tx ingest complete"
    );

    Ok(())
}

/// Insert the resource-table rows that a classified tx implies:
///
/// - `RegisterAttestorSet` -> `attestor_sets` row.
/// - `RegisterSchema` -> `schemas` row + bump `attestor_sets.schema_count`
///   + bump `address_summaries.schemas_owned_count` for the owner.
/// - `SubmitAttestation` -> `attestations` row + bump
///   `schemas.attestation_count`.
/// - Other kinds (Transfer, Unknown) -> nothing.
///
/// Each step is best-effort: an FK failure or transient Postgres
/// error on one bump doesn't abort the rest of the ingest, and the
/// tx row itself is already committed by this point. A re-index can
/// recompute counters from the source-of-truth tables (schemas /
/// attestations) if drift is observed.
async fn insert_resource_rows(
    pool: &PgPool,
    classified: &parser::ClassifiedTx,
    slot_height: u64,
    slot_timestamp: DateTime<Utc>,
) -> Result<(), IndexerError> {
    match &classified.kind {
        IndexerTx::RegisterAttestorSet(d) => {
            db::insert_attestor_set(pool, d, &classified.hash, slot_height, slot_timestamp).await?;
            // Best-effort attestor_member_count bumps; no-op in v0
            // (see db::bump_attestor_member_count doc).
            for member in &d.members {
                let _ = db::bump_attestor_member_count(pool, member).await;
            }
        }
        IndexerTx::RegisterSchema(d) => {
            db::insert_schema(pool, d, &classified.hash, slot_height, slot_timestamp).await?;
            // Bump the bound attestor set's schema_count and the
            // owner's schemas_owned_count. Failures on either don't
            // back out the schema row.
            if let Err(e) = db::bump_attestor_set_schema_count(pool, &d.attestor_set_id).await {
                warn!(
                    error = %e,
                    attestor_set_id = %d.attestor_set_id,
                    "bumping attestor_set.schema_count; counter stale"
                );
            }
            if let Err(e) = db::bump_address_schemas_owned(
                pool,
                &d.owner,
                slot_height,
                &classified.hash,
                slot_timestamp,
            )
            .await
            {
                warn!(
                    error = %e,
                    owner = %d.owner,
                    "bumping address.schemas_owned_count; counter stale"
                );
            }
        }
        IndexerTx::SubmitAttestation(d) => {
            db::insert_attestation(pool, d, &classified.hash, slot_height, slot_timestamp).await?;
            if let Err(e) = db::bump_schema_attestation_count(pool, &d.schema_id).await {
                warn!(
                    error = %e,
                    schema_id = %d.schema_id,
                    "bumping schema.attestation_count; counter stale"
                );
            }
        }
        IndexerTx::Transfer(_) | IndexerTx::Unknown { .. } => {
            // No resource rows to insert for transfers or unknown
            // kinds. Address-summary counters are maintained by
            // `update_address_summaries`.
        }
    }
    Ok(())
}

/// Update `address_summaries` counters + first/last seen for the
/// roles a tx exposes. Transfers carry sender + recipient. The
/// Attestation-module kinds (`register_attestor_set` /
/// `register_schema` / `submit_attestation`) only carry a sender
/// (the registrar / owner / submitter); their dedicated counters
/// (`schemas_owned_count`, etc.) are maintained by
/// `insert_resource_rows`.
async fn update_address_summaries(
    pool: &PgPool,
    classified: &parser::ClassifiedTx,
    slot_height: u64,
    slot_timestamp: DateTime<Utc>,
) -> Result<(), IndexerError> {
    // Resolve the sender address per-kind. None for `Unknown` since
    // the chain elides the body and we have no event-side evidence.
    let sender: Option<&str> = match &classified.kind {
        IndexerTx::Transfer(t) => Some(t.from.as_str()),
        IndexerTx::RegisterAttestorSet(d) => Some(d.registered_by.as_str()),
        IndexerTx::RegisterSchema(d) => Some(d.owner.as_str()),
        IndexerTx::SubmitAttestation(d) => Some(d.submitter.as_str()),
        IndexerTx::Unknown { .. } => None,
    };

    if let Some(addr) = sender {
        db::upsert_address_activity(
            pool,
            addr,
            AddressRole::Sender,
            slot_height,
            &classified.hash,
            slot_timestamp,
        )
        .await?;
    }

    // Receiver side. Only Transfer has a meaningful recipient.
    if let IndexerTx::Transfer(t) = &classified.kind {
        db::upsert_address_activity(
            pool,
            &t.to,
            AddressRole::Receiver,
            slot_height,
            &classified.hash,
            slot_timestamp,
        )
        .await?;
    }

    Ok(())
}
