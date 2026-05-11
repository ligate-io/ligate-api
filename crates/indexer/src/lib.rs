//! Indexer service primitives for the Ligate Chain block explorer.
//!
//! Reads the Ligate Chain REST API and writes blocks, transactions,
//! schemas, attestor sets, and attestations into Postgres so the
//! explorer can serve the list, range, and aggregate queries the
//! chain itself deliberately does not. See the chain repo's
//! [`docs/protocol/rest-api.md`] for the upstream surface.
//!
//! Ported from `ligate-io/ligate-explorer/crates/indexer/` (now
//! frontend-only) into `ligate-api` so a single Rust service hosts
//! both the indexer ingest loop AND the HTTP query endpoints
//! `explorer.ligate.io` calls. `ligate-api/crates/api` is the binary
//! that spawns [`run`] at startup alongside the axum router.
//!
//! v0 surface: slots + chain-identity bootstrap. `transactions`,
//! `schemas`, `attestor_sets`, `attestations` come in subsequent
//! migrations as the chain modules they consume stabilise.
//!
//! [`docs/protocol/rest-api.md`]:
//!   https://github.com/ligate-io/ligate-chain/blob/main/docs/protocol/rest-api.md

// Lint policy: ported types from the old standalone indexer binary
// (NodeClient deserialization shapes, IndexerError variants) pre-date
// a published-doc requirement. Skipping missing-docs lint for v0;
// tighten once the public surface is pinned by `ligate-api`'s needs.

mod client;
mod db;
mod error;
mod ingest;
mod parser;

pub use client::NodeClient;
pub use db::connect;
pub use error::IndexerError;
pub use ingest::run as run_ingest;
pub use parser::{classify_tx, outcome_of, ClassifiedTx, IndexerTransfer, IndexerTx, TxOutcome};
pub use sqlx::PgPool;

use anyhow::Result;

/// Indexer runtime config — all the state the long-running ingest loop
/// needs. The `api` binary builds one of these from env vars at
/// startup and spawns [`run`] in a tokio task.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// URL of the Ligate Chain node REST API root.
    /// e.g. `http://127.0.0.1:12346` (localnet) or
    /// `https://rpc.ligate.io` (public devnet).
    pub rpc_url: String,
    /// Postgres connection URL. The same Postgres the api's query
    /// endpoints read from.
    pub database_url: String,
    /// Slot height to start backfilling from. `None` means resume from
    /// the last indexed slot in the DB (or `1` if the DB is empty).
    pub start_height: Option<u64>,
}

/// Entry point: connect, run migrations, kick off the ingest loop.
///
/// Long-running. The `api` binary spawns this in a tokio task so the
/// HTTP server can come up immediately while the indexer backfills in
/// parallel. If the indexer task exits (via panic or returned error),
/// the api binary should log it and continue serving — chain-info +
/// drip endpoints don't depend on the indexer.
pub async fn run(cfg: IndexerConfig) -> Result<()> {
    let client = NodeClient::new(&cfg.rpc_url)?;
    let pool = connect(&cfg.database_url).await?;
    run_ingest(client, pool, cfg.start_height).await
}
