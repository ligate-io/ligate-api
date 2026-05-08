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

use ligate_api_types::{RollupInfo, SlotResponse};
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
