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

use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::client::NodeClient;
use crate::db;
use crate::error::IndexerError;

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
