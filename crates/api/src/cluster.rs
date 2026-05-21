//! `GET /v1/cluster/nodes` proxy.
//!
//! Hits the chain's internal `/v1/cluster/nodes` surface, strips private
//! VPC addresses, computes the aggregate `cluster_health`, caches the
//! result for a few seconds, and serves the public shape from
//! [`ligate_api_types::ClusterTopology`].
//!
//! Why a proxy rather than an explorer-direct call: the chain endpoint
//! is blocked at the Caddy edge (returns 404 publicly) because the
//! response includes private VPC addresses. The api lives outside the
//! VPC but inside our trust boundary; here is where the public shape
//! gets minted. Tracking issue: ligate-io/ligate-chain#442.
//!
//! Behaviour when the chain endpoint isn't reachable (404, network
//! error, malformed shape): the handler returns `cluster_health: "unknown"`
//! with an empty `nodes` list and a short `Cache-Control: max-age=10`,
//! so partner dashboards can degrade gracefully without retry-storming
//! the api during a chain incident.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use ligate_api_indexer::NodeClient;
use ligate_api_types::{ChainClusterTopology, ClusterHealth, ClusterNode, ClusterTopology};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::AppState;

/// How long the public response is reusable from cache. Five seconds
/// is the right knob because the chain side already caches the
/// underlying Postgres query for 1 s; doubling that here absorbs
/// explorer polling at 1 Hz without hammering the chain RPC, while
/// staying short enough that failover events show up within a single
/// dashboard refresh interval.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// `Cache-Control: public, max-age=` value advertised on the
/// successful response. Matches `CACHE_TTL` so downstream CDNs and
/// the explorer's `revalidate` annotation stay in sync.
const CACHE_CONTROL_MAX_AGE_SECS: u32 = 5;

/// Stale-while-revalidate window for the degraded path. Even when
/// the chain endpoint is unreachable, we hold the `unknown` body for
/// 10 seconds to avoid amplifying a chain outage into an api thunder
/// herd.
const UNKNOWN_CACHE_MAX_AGE_SECS: u32 = 10;

/// How fresh a node's heartbeat must be for the cluster to count as
/// "healthy" rather than "degraded". Set well above the cluster's
/// default heartbeat interval (100 ms) and well under the
/// `leader_timeout_millis` (500 ms) so brief network blips don't
/// flap the public status string.
const HEARTBEAT_FRESHNESS_MS: i64 = 2_000;

/// In-memory cache for the public `ClusterTopology` response. Shared
/// across requests so concurrent explorer polls reuse one chain hop.
#[derive(Clone, Default)]
pub struct ClusterCache {
    inner: Arc<RwLock<Option<(Instant, String)>>>,
}

impl ClusterCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get_fresh(&self) -> Option<String> {
        let guard = self.inner.read().await;
        let (stamped_at, body) = guard.as_ref()?;
        if stamped_at.elapsed() < CACHE_TTL {
            Some(body.clone())
        } else {
            None
        }
    }

    async fn put(&self, body: String) {
        let mut guard = self.inner.write().await;
        *guard = Some((Instant::now(), body));
    }
}

/// `GET /v1/cluster/nodes` handler.
pub async fn nodes(State(state): State<AppState>) -> Response {
    if let Some(cached) = state.cluster_cache.get_fresh().await {
        return cached_json(cached, CACHE_CONTROL_MAX_AGE_SECS, StatusCode::OK);
    }

    let topology = fetch_and_transform(&state).await;
    let body = match serde_json::to_string(&topology) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                ?err,
                "/v1/cluster/nodes: failed to serialize topology response"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "serialization error").into_response();
        }
    };
    let max_age = if matches!(topology.cluster_health, ClusterHealth::Unknown) {
        UNKNOWN_CACHE_MAX_AGE_SECS
    } else {
        CACHE_CONTROL_MAX_AGE_SECS
    };
    let status = if matches!(topology.cluster_health, ClusterHealth::Unknown) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    // Cache successes for the standard TTL. Don't cache "unknown" past
    // the response's own Cache-Control window; if the chain comes back
    // we want the next request to retry promptly.
    if matches!(
        topology.cluster_health,
        ClusterHealth::Healthy | ClusterHealth::Degraded | ClusterHealth::Leaderless
    ) {
        state.cluster_cache.put(body.clone()).await;
    }
    cached_json(body, max_age, status)
}

/// Fetch the chain's cluster topology and transform into the public
/// shape. Always returns a [`ClusterTopology`] (never panics or
/// errors out) so the handler can serve a deterministic shape; chain
/// failures degrade to `ClusterHealth::Unknown` with an empty node
/// list.
async fn fetch_and_transform(state: &AppState) -> ClusterTopology {
    let now_unix_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0);

    let client = match NodeClient::new(&state.config.chain_rpc) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "/v1/cluster/nodes: failed to build NodeClient");
            return unknown_topology(now_unix_ms);
        }
    };

    let chain_topology = match client
        .cluster_nodes(state.config.chain_cluster_auth_token.as_deref())
        .await
    {
        Ok(t) => t,
        Err(err) => {
            // Distinguish 404 (Caddy-blocked or chain hasn't
            // shipped #442 yet) from real errors in the log; both
            // surface to clients as Unknown.
            debug!(
                ?err,
                "/v1/cluster/nodes: chain RPC fetch failed; serving cluster_health=unknown"
            );
            return unknown_topology(now_unix_ms);
        }
    };

    transform(chain_topology, now_unix_ms)
}

/// Strip private addresses + compute the aggregate health string.
/// Pure function so it's covered by unit tests without spawning a
/// fake chain server.
fn transform(chain: ChainClusterTopology, now_unix_ms: i64) -> ClusterTopology {
    let nodes: Vec<ClusterNode> = chain
        .nodes
        .into_iter()
        .map(|n| ClusterNode {
            node_id: n.node_id,
            is_leader: n.is_leader,
            last_heartbeat_age_ms: n.last_heartbeat_age_ms,
        })
        .collect();

    let cluster_health = if chain.leader_node_id.is_none() {
        ClusterHealth::Leaderless
    } else if nodes
        .iter()
        .any(|n| n.last_heartbeat_age_ms > HEARTBEAT_FRESHNESS_MS)
    {
        ClusterHealth::Degraded
    } else {
        ClusterHealth::Healthy
    };

    ClusterTopology {
        nodes,
        leader_node_id: chain.leader_node_id,
        leader_acquired_at_epoch_ms: chain.leader_acquired_at_epoch_ms,
        generated_at_epoch_ms: now_unix_ms,
        cluster_health,
    }
}

fn unknown_topology(now_unix_ms: i64) -> ClusterTopology {
    ClusterTopology {
        nodes: Vec::new(),
        leader_node_id: None,
        leader_acquired_at_epoch_ms: None,
        generated_at_epoch_ms: now_unix_ms,
        cluster_health: ClusterHealth::Unknown,
    }
}

/// Render a `Cache-Control`-tagged JSON response. Mirrors the helper
/// in `stats.rs`; lifted here so the cluster module stays
/// self-contained.
fn cached_json(body: String, max_age_secs: u32, status: StatusCode) -> Response {
    let cache_control = format!("public, max-age={max_age_secs}");
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, cache_control.as_str()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ligate_api_types::ChainClusterNode;

    fn chain_topology_with(
        nodes: Vec<ChainClusterNode>,
        leader: Option<&str>,
    ) -> ChainClusterTopology {
        ChainClusterTopology {
            nodes,
            leader_node_id: leader.map(String::from),
            leader_acquired_at_epoch_ms: leader.map(|_| 1_000_000),
            generated_at_epoch_ms: 1_000_100,
        }
    }

    fn node(id: &str, addr: &str, is_leader: bool, age_ms: i64) -> ChainClusterNode {
        ChainClusterNode {
            node_id: id.to_string(),
            address: addr.to_string(),
            is_leader,
            last_heartbeat_age_ms: age_ms,
        }
    }

    #[test]
    fn transform_strips_addresses() {
        let chain = chain_topology_with(
            vec![
                node("ligate-1", "10.128.0.3:12346", true, 30),
                node("ligate-2", "10.128.0.6:12346", false, 50),
            ],
            Some("ligate-1"),
        );
        let public = transform(chain, 2_000_000);
        assert_eq!(public.nodes.len(), 2);
        assert_eq!(public.leader_node_id.as_deref(), Some("ligate-1"));
        // Public ClusterNode has no `address` field; the test is
        // existence-by-type. Just sanity-check the surviving fields.
        assert_eq!(public.nodes[0].node_id, "ligate-1");
        assert!(public.nodes[0].is_leader);
        assert_eq!(public.nodes[0].last_heartbeat_age_ms, 30);
        assert_eq!(public.generated_at_epoch_ms, 2_000_000);
    }

    #[test]
    fn cluster_health_healthy_when_all_fresh() {
        let chain = chain_topology_with(
            vec![
                node("ligate-1", "addr", true, 30),
                node("ligate-2", "addr", false, 80),
            ],
            Some("ligate-1"),
        );
        let public = transform(chain, 0);
        assert_eq!(public.cluster_health, ClusterHealth::Healthy);
    }

    #[test]
    fn cluster_health_degraded_when_one_stale() {
        let chain = chain_topology_with(
            vec![
                node("ligate-1", "addr", true, 30),
                node("ligate-2", "addr", false, 5_000),
            ],
            Some("ligate-1"),
        );
        let public = transform(chain, 0);
        assert_eq!(public.cluster_health, ClusterHealth::Degraded);
    }

    #[test]
    fn cluster_health_leaderless_when_no_leader() {
        let chain = chain_topology_with(vec![node("ligate-1", "addr", false, 30)], None);
        let public = transform(chain, 0);
        assert_eq!(public.cluster_health, ClusterHealth::Leaderless);
    }

    #[test]
    fn unknown_topology_has_empty_nodes_and_unknown_health() {
        let t = unknown_topology(123);
        assert!(t.nodes.is_empty());
        assert_eq!(t.cluster_health, ClusterHealth::Unknown);
        assert_eq!(t.generated_at_epoch_ms, 123);
        assert!(t.leader_node_id.is_none());
    }
}
