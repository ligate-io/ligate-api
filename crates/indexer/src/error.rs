//! Error type for the indexer service.
//!
//! Most callers just propagate via `anyhow::Error` since the indexer
//! is a long-running service whose every failure mode is "log and
//! retry". The typed errors here exist so unit tests and operational
//! tooling can match on specific failures (e.g., distinguish
//! "node-unreachable" from "node-returned-bad-shape" so the retry
//! backoff can differ).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexerError {
    /// Node is unreachable (connection refused, DNS, timeout). Retry
    /// with backoff. Operationally this means "node is restarting" or
    /// "network blip".
    #[error("node unreachable: {0}")]
    NodeUnreachable(#[source] reqwest::Error),

    /// Node responded but the body shape doesn't match what we
    /// expect. Typically means an SDK upgrade we haven't tracked.
    /// Surface to operators rather than retrying silently.
    #[error("node returned unexpected body shape at {url}: {source}")]
    NodeBadShape {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    /// Database write or query failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Migrations failed to apply on startup.
    #[error("running migrations: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T, E = IndexerError> = std::result::Result<T, E>;
