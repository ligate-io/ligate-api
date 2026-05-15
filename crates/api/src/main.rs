//! `ligate-api` — unified HTTP API for Ligate Chain.
//!
//! One Rust service backing `api.ligate.io`, hosting:
//!
//! - **Drip (faucet)** — `POST /v1/drip`, `GET /v1/drip/status`. Hot-key
//!   signs a `bank.transfer` to the requesting address, rate-limited
//!   per-address. Ported from the (now-archived) `ligate-io/faucet`
//!   repo.
//! - **Indexer queries** — `GET /v1/blocks*`, `/v1/txs*`,
//!   `/v1/addresses/*`, `/v1/schemas*`, `/v1/info`. Postgres-backed; the
//!   indexer task running in the same process keeps the DB current.
//!
//! Deploys to Railway: single Dockerfile, single Postgres connection,
//! single env-var bag. The `ligate-explorer` Next.js frontend at
//! `explorer.ligate.io` calls this service for everything it shows.
//!
//! ## Boot sequence
//!
//! 1. Parse env (chain RPC, Postgres URL, drip signer key, drip params).
//! 2. Connect Postgres + run sqlx migrations.
//! 3. Spawn the indexer ingest task in the background.
//! 4. Build the drip [`Signer`] with hot key + chain identity.
//! 5. Mount the axum router and bind on `API_BIND` (default `0.0.0.0:8080`).
//!
//! ## Failure modes
//!
//! - Postgres unreachable → fail fast at boot.
//! - Indexer task panics → logged, api keeps serving (`/v1/drip` and
//!   `/v1/info` don't depend on the indexer; the explorer's block /
//!   tx pages will return stale data until the indexer recovers, which
//!   is acceptable degradation).
//! - Chain RPC unreachable → drip endpoints return 502, indexer task
//!   retries with backoff.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use ligate_api_drip::{RateLimiter, Signer};
use ligate_api_indexer::IndexerConfig;
use sov_bank::TokenId;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

mod config;
mod cursor;
mod handlers;
mod queries;
mod responses;
mod stats;

use crate::config::Config;

/// Shared application state. Cloneable; held inside `axum::Router::with_state`
/// so each request handler gets a `State<AppState>` extractor.
#[derive(Clone)]
pub(crate) struct AppState {
    pub config: Arc<Config>,
    pub signer: Arc<Signer>,
    pub rate_limiter: Arc<RateLimiter>,
    /// Postgres pool. Indexer task writes to this; query handlers read
    /// from it. Connection limits configured in `Config::pg_pool_size`.
    pub pg: sqlx::PgPool,
    /// 30s in-process cache for `/v1/stats/*` responses. Bounds the
    /// Postgres + chain-RPC load that concurrent Grafana scrapes can
    /// put on the api (every dashboard panel for every viewer would
    /// otherwise hit DB on every refresh).
    pub stats_cache: stats::StatsCache,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Structured JSON logs by default. Override via `RUST_LOG` for
    // local development.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ligate_api=info,sqlx=warn".into()),
        )
        .json()
        .init();

    let config = Config::from_env().context("loading config from env")?;
    let bind: SocketAddr = config
        .bind
        .parse()
        .context("parsing API_BIND as SocketAddr")?;

    info!(?bind, "ligate-api starting");

    // Postgres pool — drip rate-limit history + indexer reads.
    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.pg_pool_size)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await
        .with_context(|| {
            format!(
                "connecting to Postgres at {}",
                redact_pg_url(&config.database_url)
            )
        })?;

    info!("postgres connected, running migrations");
    sqlx::migrate!("../../migrations")
        .run(&pg)
        .await
        .context("running sqlx migrations")?;

    // Build the drip signer.
    let token_id_bytes =
        hex::decode(&config.lgt_token_id_hex).context("LGT_TOKEN_ID must be valid hex")?;
    let lgt_token_id = TokenId::try_from(token_id_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("LGT_TOKEN_ID wrong shape: {e:?}"))?;
    // Seed the in-memory nonce counter from chain unless the operator
    // explicitly pinned it via `DRIP_STARTING_NONCE`. Auto-seeding is
    // what makes a Railway redeploy safe: without it, the counter
    // resets to 0 every restart while the on-chain nonce is at N, and
    // the next drip after each redeploy 4xx's with `Tx bad nonce`.
    let (signer, starting_nonce) = Signer::new_with_chain_seed(
        &config.drip_signer_key,
        config.chain_rpc.clone(),
        config.chain_id,
        config.chain_hash,
        lgt_token_id,
        config.drip_starting_nonce,
    )
    .await
    .context("building drip signer")?;

    info!(
        faucet_address = %signer.address(),
        starting_nonce,
        nonce_source = if config.drip_starting_nonce.is_some() {
            "DRIP_STARTING_NONCE override"
        } else {
            "chain (auto-seeded)"
        },
        "drip signer loaded",
    );

    // Optional drip-budget sanity check — refuse to start if the
    // signer's balance covers fewer than `MIN_DRIPS_BUDGET` drips. Set
    // `DRIP_MIN_BUDGET=0` to skip (e.g. for first-boot when funding
    // hasn't happened yet).
    if config.drip_min_budget > 0 {
        match query_balance_with_retry(&signer, 5, Duration::from_secs(2)).await {
            Ok(balance) => {
                let budget = balance / config.drip_amount;
                if (budget as u64) < config.drip_min_budget {
                    anyhow::bail!(
                        "drip signer balance ({balance} nano-LGT) covers only {budget} drips at \
                         {} nano-LGT/drip; minimum is {} (DRIP_MIN_BUDGET). Fund the signer or \
                         lower DRIP_AMOUNT before starting.",
                        config.drip_amount,
                        config.drip_min_budget,
                    );
                }
                info!(
                    balance,
                    drip_amount = config.drip_amount,
                    budget,
                    "drip-budget check OK"
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "drip-budget check failed: {e:#}. Set DRIP_MIN_BUDGET=0 to skip if the chain \
                     is intentionally unreachable at boot."
                );
            }
        }
    } else {
        warn!("DRIP_MIN_BUDGET=0 — skipping startup drip-budget check");
    }

    let rate_limiter = RateLimiter::new(Duration::from_secs(config.drip_rate_limit_secs));

    let state = AppState {
        config: Arc::new(config.clone()),
        signer: Arc::new(signer),
        rate_limiter: Arc::new(rate_limiter),
        pg: pg.clone(),
        stats_cache: stats::StatsCache::new(),
    };

    // Spawn the indexer ingest task in the background. If it panics or
    // returns an error, log it; the api stays up serving /v1/drip and
    // proxy endpoints.
    let indexer_cfg = IndexerConfig {
        rpc_url: config.chain_rpc.clone(),
        database_url: config.database_url.clone(),
        start_height: config.indexer_start_height,
    };
    tokio::spawn(async move {
        if let Err(e) = ligate_api_indexer::run(indexer_cfg).await {
            tracing::error!(error = ?e, "indexer task exited; api continues serving");
        }
    });

    // Permissive CORS — partner web apps (Mneme, Themisra,
    // explorer.ligate.io itself) hit api.ligate.io from arbitrary
    // origins. Tighten the allow-list at testnet+; for devnet, the
    // public-permissionless story matches "anyone can hit any
    // endpoint from any browser".
    let cors = CorsLayer::permissive();

    let app = Router::new()
        // Operator probes — keep unversioned per orchestrator
        // convention.
        .route("/health", get(handlers::health))
        // v1 surface — chain identity, drip, indexer queries.
        .route("/v1/health", get(handlers::health))
        .route("/v1/info", get(handlers::info))
        .route("/v1/drip", post(handlers::drip))
        .route("/v1/drip/status", get(handlers::drip_status))
        // Indexer queries — stubbed in v0; flesh out in subsequent PRs
        // once the indexer's slot/tx schemas stabilise.
        .route("/v1/blocks", get(handlers::blocks_list))
        .route("/v1/blocks/{height}", get(handlers::block_by_height))
        .route("/v1/txs", get(handlers::txs_list))
        .route("/v1/txs/{hash}", get(handlers::tx_by_hash))
        .route("/v1/addresses/{addr}", get(handlers::address_summary))
        .route("/v1/schemas", get(handlers::schemas_list))
        .route("/v1/schemas/{id}", get(handlers::schema_by_id))
        .route("/v1/attestor-sets/{id}", get(handlers::attestor_set_by_id))
        // Aggregate analytics for the explorer + investor dashboard.
        // All cached 30s in-process; see `stats::StatsCache`.
        .route("/v1/stats/totals", get(stats::totals))
        .route("/v1/stats/active-addresses", get(stats::active_addresses))
        .route("/v1/stats/new-wallets-daily", get(stats::new_wallets_daily))
        .route("/v1/stats/tx-rate-daily", get(stats::tx_rate_daily))
        .route("/v1/stats/top-holders", get(stats::top_holders))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding listener at {bind}"))?;
    info!(?bind, "ligate-api listening");
    axum::serve(listener, app).await.context("axum serve")?;

    Ok(())
}

/// Query the drip signer's own LGT balance with bounded retries.
/// Mirrors the pattern in the (now-archived) faucet repo's main.rs.
async fn query_balance_with_retry(
    signer: &Signer,
    max_attempts: u32,
    backoff: Duration,
) -> Result<u128> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..max_attempts {
        match signer.query_self_balance().await {
            Ok(b) => return Ok(b),
            Err(e) => {
                warn!(?e, attempt, "drip balance query failed; retrying");
                last_err = Some(e);
                tokio::time::sleep(backoff).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("balance query failed with no error captured")))
}

/// Strip credentials from a Postgres URL for log lines.
fn redact_pg_url(url: &str) -> String {
    if let Some(at) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            return format!("{}://[redacted]@{}", &url[..scheme_end], &url[at + 1..]);
        }
    }
    url.to_string()
}
