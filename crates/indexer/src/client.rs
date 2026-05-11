//! Thin REST client over `ligate-node`'s `/v1/...` surface.
//!
//! Wraps `reqwest::Client` and lifts JSON shapes from
//! [`ligate_api_types`] (which mirrors the chain's
//! `docs/protocol/rest-api.md`). Read-only — submission goes
//! through `ligate-client::submit` in the chain repo, not here.
//!
//! ## Why a typed wrapper instead of raw `reqwest`
//!
//! 1. Localizes the `/v1/` URL composition. If the prefix changes
//!    (or we add `/v2/` per the upgrade policy in
//!    `docs/protocol/upgrades.md`), one place to update.
//! 2. Makes ingest-loop code testable. `mockito` swaps the URL.
//! 3. Maps reqwest's `StatusCode + body` mess into our typed
//!    [`IndexerError`] cleanly.
//!
//! Endpoints used in v0:
//!
//! - `GET /v1/rollup/info` — chain identity bootstrap on startup.
//! - `GET /v1/ledger/slots/latest` — driver of the live-tail loop.
//! - `GET /v1/ledger/slots/{height}` — driver of the backfill loop.

use ligate_api_types::{LedgerBatch, LedgerEvent, LedgerTx, RollupInfo, SlotResponse};
use reqwest::Client as Http;
use url::Url;

use crate::error::{IndexerError, Result};

#[derive(Debug, Clone)]
pub struct NodeClient {
    http: Http,
    base: Url,
}

impl NodeClient {
    /// Construct a client pointing at the given node REST base URL.
    ///
    /// Accepts e.g. `http://127.0.0.1:12346` or `https://rpc.ligate.io`
    /// (with or without trailing slash). The `/v1/` prefix is added
    /// per call site.
    pub fn new(base_url: &str) -> Result<Self> {
        let normalized = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        let base = Url::parse(&normalized).map_err(|e| IndexerError::NodeBadShape {
            url: base_url.to_string(),
            source: serde_json::Error::custom(format!("URL parse: {e}")),
        })?;
        Ok(Self {
            http: Http::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client builder is infallible with default config"),
            base,
        })
    }

    /// `GET /v1/rollup/info`. Returns the chain identity tuple.
    pub async fn rollup_info(&self) -> Result<RollupInfo> {
        let url = self.url("v1/rollup/info");
        let bytes = self.fetch(&url).await?;
        serde_json::from_slice::<RollupInfo>(&bytes).map_err(|source| IndexerError::NodeBadShape {
            url: url.to_string(),
            source,
        })
    }

    /// `GET /v1/ledger/slots/latest`. Returns the latest produced slot.
    pub async fn latest_slot(&self) -> Result<SlotResponse> {
        let url = self.url("v1/ledger/slots/latest");
        let bytes = self.fetch(&url).await?;
        serde_json::from_slice::<SlotResponse>(&bytes).map_err(|source| {
            IndexerError::NodeBadShape {
                url: url.to_string(),
                source,
            }
        })
    }

    /// `GET /v1/ledger/slots/{height}`. Returns one slot by height.
    /// Returns `Ok(None)` if the chain hasn't produced that slot yet.
    pub async fn slot_at(&self, height: u64) -> Result<Option<SlotResponse>> {
        let url = self.url(&format!("v1/ledger/slots/{height}"));
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(IndexerError::NodeUnreachable)?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let bytes = resp.bytes().await.map_err(IndexerError::NodeUnreachable)?;
        let parsed = serde_json::from_slice::<SlotResponse>(&bytes).map_err(|source| {
            IndexerError::NodeBadShape {
                url: url.to_string(),
                source,
            }
        })?;
        Ok(Some(parsed))
    }

    /// `GET /v1/ledger/batches/{number}`. Returns batch by number.
    /// `None` if the chain doesn't have that batch yet (404).
    pub async fn batch_at(&self, number: u64) -> Result<Option<LedgerBatch>> {
        let url = self.url(&format!("v1/ledger/batches/{number}"));
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(IndexerError::NodeUnreachable)?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let bytes = resp.bytes().await.map_err(IndexerError::NodeUnreachable)?;
        let parsed = serde_json::from_slice::<LedgerBatch>(&bytes).map_err(|source| {
            IndexerError::NodeBadShape {
                url: url.to_string(),
                source,
            }
        })?;
        Ok(Some(parsed))
    }

    /// `GET /v1/ledger/txs/{number}`. Returns tx by global number.
    /// `None` if 404.
    ///
    /// The chain accepts both numeric tx-numbers and the bech32m
    /// `ltx1...` hash on this path (via `TxId::FromStr`). The indexer
    /// walks numerically to match how batches expose `tx_range`,
    /// avoiding a second hash-lookup roundtrip per tx.
    pub async fn tx_at_number(&self, number: u64) -> Result<Option<LedgerTx>> {
        let url = self.url(&format!("v1/ledger/txs/{number}"));
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(IndexerError::NodeUnreachable)?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let bytes = resp.bytes().await.map_err(IndexerError::NodeUnreachable)?;
        let parsed = serde_json::from_slice::<LedgerTx>(&bytes).map_err(|source| {
            IndexerError::NodeBadShape {
                url: url.to_string(),
                source,
            }
        })?;
        Ok(Some(parsed))
    }

    /// `GET /v1/ledger/slots/{slotId}/events`. Returns all events
    /// emitted during slot execution. Each event carries its
    /// originating `tx_hash`, so callers can group by tx.
    ///
    /// Returns an empty vec on 404 (slot with no events, or chain
    /// returns 404 for a slot that didn't emit any). The chain
    /// surface is well-defined here — a slot that exists has its
    /// events endpoint reachable; an empty page comes back as
    /// `[]`, not 404. Treating 404 as empty is a forward-compat
    /// guard against the chain switching to that shape later.
    pub async fn events_for_slot(&self, slot_height: u64) -> Result<Vec<LedgerEvent>> {
        let url = self.url(&format!("v1/ledger/slots/{slot_height}/events"));
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(IndexerError::NodeUnreachable)?;
        if resp.status().as_u16() == 404 {
            return Ok(Vec::new());
        }
        let bytes = resp.bytes().await.map_err(IndexerError::NodeUnreachable)?;
        // The chain serialises this endpoint as `[event, event, ...]`
        // — a bare JSON array, not an envelope. Deserialise directly.
        serde_json::from_slice::<Vec<LedgerEvent>>(&bytes).map_err(|source| {
            IndexerError::NodeBadShape {
                url: url.to_string(),
                source,
            }
        })
    }

    /// Build a fully-qualified URL by joining `path` onto `self.base`.
    fn url(&self, path: &str) -> Url {
        // `Url::join` drops the existing path segment unless we've
        // ensured a trailing slash, which we did in `new`. So this
        // appends correctly: base="http://x:12346/" + "v1/foo" =
        // "http://x:12346/v1/foo".
        self.base
            .join(path)
            .expect("static suffix joined onto a parsed base URL")
    }

    /// Issue a GET, propagate transport / shape errors uniformly.
    /// Doesn't decode JSON — caller picks the type.
    async fn fetch(&self, url: &Url) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(IndexerError::NodeUnreachable)?;
        let bytes = resp.bytes().await.map_err(IndexerError::NodeUnreachable)?;
        Ok(bytes.to_vec())
    }
}

// `serde_json::Error::custom` lives behind the `serde::de::Error`
// trait at the import site; pull that in.
use serde::de::Error as _;

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[tokio::test]
    async fn rollup_info_parses_canonical_body() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("GET", "/v1/rollup/info")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"chain_id":"ligate-devnet-1","chain_hash":"abcd","version":"0.0.1"}"#)
            .create_async()
            .await;

        let client = NodeClient::new(&srv.url()).unwrap();
        let info = client.rollup_info().await.unwrap();
        assert_eq!(info.chain_id, "ligate-devnet-1");
        assert_eq!(info.chain_hash, "abcd");
        assert_eq!(info.version, "0.0.1");
    }

    #[tokio::test]
    async fn slot_at_returns_none_on_404() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("GET", "/v1/ledger/slots/999")
            .with_status(404)
            .create_async()
            .await;

        let client = NodeClient::new(&srv.url()).unwrap();
        let slot = client.slot_at(999).await.unwrap();
        assert!(slot.is_none());
    }

    #[tokio::test]
    async fn batch_at_parses_canonical_body() {
        let mut srv = Server::new_async().await;
        // Mirrors the slot-100-batch-0.json fixture shape; the
        // typed `LedgerBatch` must round-trip the chain's emitted
        // body without losing the receipt subtree (which lives in
        // `raw` via #[serde(flatten)]).
        let _m = srv
            .mock("GET", "/v1/ledger/batches/95")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"type":"batch","number":95,"hash":"lblk1mcfgumhvyx2wpglq6he4rgdr4alv80aq7qjmm3ljpj7trj3kflwq7dv0z5","tx_range":{"start":1,"end":3},"slot_number":100,"receipt":{"gas_used":[0,0]}}"#,
            )
            .create_async()
            .await;

        let client = NodeClient::new(&srv.url()).unwrap();
        let batch = client.batch_at(95).await.unwrap().expect("batch present");
        assert_eq!(batch.number, 95);
        assert_eq!(batch.slot_number, 100);
        assert_eq!(batch.tx_range.start, 1);
        assert_eq!(batch.tx_range.end, 3);
        assert!(batch.raw.contains_key("receipt"));
    }

    #[tokio::test]
    async fn batch_at_returns_none_on_404() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("GET", "/v1/ledger/batches/999")
            .with_status(404)
            .create_async()
            .await;
        let client = NodeClient::new(&srv.url()).unwrap();
        assert!(client.batch_at(999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tx_at_number_parses_canonical_body() {
        let mut srv = Server::new_async().await;
        // Mirrors tx-by-hash.json: empty body, gas_used in receipt.
        let _m = srv
            .mock("GET", "/v1/ledger/txs/1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"type":"tx","number":1,"hash":"ltx19zwttsdksue0ef4fan7lnfhcjdq9lq8d592hjpcc30gh5c77ytzqvjmjm4","event_range":{"start":1,"end":2},"body":{"data":"","sequencing_data":null},"receipt":{"result":"successful","data":{"gas_used":[16806,16806]}},"batch_number":8929}"#,
            )
            .create_async()
            .await;

        let client = NodeClient::new(&srv.url()).unwrap();
        let tx = client.tx_at_number(1).await.unwrap().expect("tx present");
        assert_eq!(tx.number, 1);
        assert_eq!(tx.batch_number, 8929);
        assert_eq!(tx.receipt.result, "successful");
        assert!(tx.hash.starts_with("ltx1"));
    }

    #[tokio::test]
    async fn events_for_slot_parses_array_body() {
        let mut srv = Server::new_async().await;
        // The endpoint returns a bare JSON array of events. Each
        // event has the `tx_hash` field the indexer uses to group
        // events by tx during classification.
        let _m = srv
            .mock("GET", "/v1/ledger/slots/100/events")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{"type":"event","number":1,"key":"Bank/TokenTransferred","value":{"token_transferred":{"from":{"user":"lig132yw8ht5p8cetl2jmvknewjawt9xwzdlrk2pyxlnwjyqz3m499u"},"to":{"user":"lig13x2xvtj2n3g5zdrc2g27uswja0e9dllxlu33y8estm0gw4dhs6d"},"coins":{"amount":"1000000000","token_id":"token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7"}}},"module":{"type":"moduleRef","name":"Bank"},"tx_hash":"ltx19zwttsdksue0ef4fan7lnfhcjdq9lq8d592hjpcc30gh5c77ytzqvjmjm4"}]"#,
            )
            .create_async()
            .await;

        let client = NodeClient::new(&srv.url()).unwrap();
        let events = client.events_for_slot(100).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, "Bank/TokenTransferred");
        assert!(events[0].tx_hash.starts_with("ltx1"));
    }

    #[tokio::test]
    async fn events_for_slot_treats_404_as_empty() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("GET", "/v1/ledger/slots/999/events")
            .with_status(404)
            .create_async()
            .await;
        let client = NodeClient::new(&srv.url()).unwrap();
        assert!(client.events_for_slot(999).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn url_joining_handles_trailing_slash() {
        // Both `http://x:12346` and `http://x:12346/` should produce
        // the same final path when joined.
        let a = NodeClient::new("http://x.example:12346").unwrap();
        let b = NodeClient::new("http://x.example:12346/").unwrap();
        assert_eq!(a.url("v1/rollup/info"), b.url("v1/rollup/info"));
        assert!(a
            .url("v1/rollup/info")
            .as_str()
            .ends_with("/v1/rollup/info"));
    }
}
