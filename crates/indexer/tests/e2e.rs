//! End-to-end smoke test for the indexer.
//!
//! Verifies the full ingest path against a real Postgres:
//!
//! 1. Boots a `mockito` HTTP server stubbing the chain REST surface
//!    that the indexer's `NodeClient` reads.
//! 2. Connects to Postgres at `DATABASE_URL` (CI sets this via the
//!    service container; locally, point it at any dev Postgres with
//!    migrations applied).
//! 3. Spawns the indexer's `run()` loop pointed at the mock chain +
//!    real DB.
//! 4. Polls `SELECT COUNT(*) FROM slots` until > 0 (with a deadline).
//! 5. Asserts the indexer wrote the chain identity row + at least one
//!    slot row.
//! 6. Aborts the indexer task.
//!
//! Why a mocked chain, not a real `ligate-node` binary in CI:
//!
//! - The chain binary's cold-cache build is ~10 min in CI. With cargo
//!   cache hits, still 2-3 min. The marginal value of testing real
//!   `ligate-node` HTTP responses (over canned ones) is low: the
//!   indexer's typed deserialise paths are already covered by unit
//!   tests in `client.rs`. The thing the unit tests CAN'T cover is
//!   "do the rows actually land in Postgres" — which is what this
//!   test proves, against a real DB.
//! - When the chain ships its own docker image
//!   (https://github.com/ligate-io/ligate-chain/issues/280), a second
//!   `e2e-chain` job can layer on top of this one without removing
//!   the mockito-driven coverage.
//!
//! Skipped (not failed) when `DATABASE_URL` is unset — keeps `cargo
//! test` green for the local-without-Postgres development flow.

use std::time::{Duration, Instant};

use ligate_api_indexer::{run, IndexerConfig};
use mockito::Server;
use sqlx::PgPool;

const DATABASE_URL_VAR: &str = "DATABASE_URL";

/// Poll deadline. The ingest loop's idle tail-poll is 2s; one slot
/// should land in <5s on a healthy Postgres. 30s is the comfort
/// margin for slow CI runners.
const POLL_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
async fn ingest_loop_writes_chain_identity_and_first_slot() {
    let Ok(database_url) = std::env::var(DATABASE_URL_VAR) else {
        eprintln!("skipping: {DATABASE_URL_VAR} not set");
        return;
    };

    // Fresh pool + clean tables. Each run starts from a known-empty
    // state so the assertions don't have to handle prior rows.
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to DATABASE_URL");
    truncate_indexer_state(&pool).await;

    // ----- Mock chain ---------------------------------------------
    //
    // Three routes are enough for an empty-slot ingest: chain info,
    // head pointer, slot detail. With `batch_range = {0,0}` on the
    // slot, the inner `ingest_slot_transactions` short-circuits before
    // touching batches/txs/events.
    let mut srv = Server::new_async().await;
    let _info_mock = srv
        .mock("GET", "/v1/rollup/info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"chain_id":"ligate-e2e-test","chain_hash":"lsch1abcdefghijklmnopqrstuvwxyz0123456789","version":"0.0.1-e2e"}"#,
        )
        .expect_at_least(1)
        .create_async()
        .await;

    let _latest_mock = srv
        .mock("GET", "/v1/ledger/slots/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(canned_slot_body(1))
        .expect_at_least(1)
        .create_async()
        .await;

    let _slot1_mock = srv
        .mock("GET", "/v1/ledger/slots/1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(canned_slot_body(1))
        .expect_at_least(1)
        .create_async()
        .await;

    // ----- Spawn the indexer -------------------------------------
    let cfg = IndexerConfig {
        rpc_url: srv.url(),
        database_url: database_url.clone(),
        start_height: Some(1),
    };
    let handle = tokio::spawn(async move { run(cfg).await });

    // ----- Poll for rows -----------------------------------------
    let deadline = Instant::now() + POLL_DEADLINE;
    let mut slots_count: i64 = 0;
    while Instant::now() < deadline {
        slots_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM slots")
            .fetch_one(&pool)
            .await
            .expect("count slots");
        if slots_count > 0 {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    handle.abort();
    let _ = handle.await;

    assert!(
        slots_count > 0,
        "indexer didn't write any slot rows within {POLL_DEADLINE:?}"
    );

    // Chain identity should have been bootstrapped to indexer_state.
    let chain_id: Option<String> = sqlx::query_scalar(
        "SELECT v FROM indexer_state WHERE k = 'chain_id'",
    )
    .fetch_optional(&pool)
    .await
    .expect("read chain_id from indexer_state");
    assert_eq!(chain_id.as_deref(), Some("ligate-e2e-test"));
}

/// A canned `SlotResponse` body that's just enough to satisfy
/// `ligate_api_types::SlotResponse` deserialisation. Empty
/// `batch_range` means no batches/txs/events to fetch; ingest is
/// effectively just "upsert this row".
fn canned_slot_body(height: u64) -> String {
    format!(
        r#"{{
            "type": "slot",
            "number": {height},
            "hash": "lblk1{}",
            "state_root": "lsr1{}",
            "batch_range": {{ "start": 0, "end": 0 }},
            "finality_status": "finalized",
            "timestamp": 1778290684471
        }}"#,
        "z".repeat(58),
        "z".repeat(102),
    )
}

async fn truncate_indexer_state(pool: &PgPool) {
    // Order-aware truncate: clear everything that the indexer might
    // write, in FK-safe order. `CASCADE` covers any FKs we add later
    // without this test needing updates.
    let tables = [
        "address_summaries",
        "attestations",
        "schemas",
        "attestor_sets",
        "transactions",
        "slots",
        "indexer_state",
    ];
    for t in tables {
        sqlx::query(&format!("TRUNCATE TABLE {t} CASCADE"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("truncate {t}: {e}"));
    }
}
