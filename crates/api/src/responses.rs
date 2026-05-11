//! Response shapes for the v1 indexer query endpoints.
//!
//! Pinned to RFC 0002 (`docs/rfc/0002-response-shapes.md`). Three
//! repos consume these shapes (ligate-js, ligate-explorer, partner
//! integrations); changing them after partners ship is expensive, so
//! prefer "add a field" over "rename a field" when the chain emits
//! new data.
//!
//! Encoding rules (per RFC 0002):
//!
//! - u128 amounts → decimal string (JSON `number` loses precision past 2^53)
//! - u64 / i64 → JSON number (chain values comfortably fit in f64)
//! - timestamps → RFC3339 with millisecond precision (`2026-05-09T01:23:45.678Z`)
//! - tx / block hashes → bech32m (`ltx1...`, `lblk1...`) since `ligate-chain@0ac7e5b`
//! - identifiers → bech32m (`lig1`, `lsc1`, `las1`, `lph1`, `lpk1`, `token_1`)
//! - optional fields → always present, `null` when absent
//!
//! List endpoints wrap their data in [`Page`] per RFC 0001's cursor
//! pagination envelope. Single-resource endpoints unwrap the resource
//! directly.

use serde::Serialize;

/// Chain identity + indexer head, served at `GET /v1/info`.
///
/// Combines `GET /v1/rollup/info` from the chain (`chain_id`,
/// `chain_hash`, `version`) with indexer-side fields the explorer
/// needs to render a "catching up" badge (`indexer_height`,
/// `head_height`, `head_lag_slots`). Partners who only care about
/// chain identity can ignore the indexer fields.
#[derive(Debug, Serialize)]
pub struct InfoResponse {
    /// Cosmos-style chain id from the `[chain]` config block. E.g.
    /// `ligate-localnet`, `ligate-devnet-1`, `ligate-1`.
    pub chain_id: String,
    /// Build-time `CHAIN_HASH`. Bech32m-encoded with HRP `lsch`
    /// (`lsch1...`) since `ligate-chain@0ac7e5b`; matches the SDK's
    /// `/v1/rollup/schema`. Wallets use it as the chain-identity
    /// fingerprint at signing time.
    pub chain_hash: String,
    /// `ligate-node` binary semver.
    pub version: String,
    /// Highest slot the indexer has fully ingested (and persisted to
    /// Postgres). `null` only when the indexer has never written a
    /// slot — fresh boot, no progress yet.
    pub indexer_height: Option<u64>,
    /// Highest slot the chain has finalised. Can be larger than
    /// `indexer_height` while the indexer catches up after a restart
    /// or backlog. `null` only when the chain proxy call failed in
    /// the same request as info-rendering — we'd rather degrade
    /// gracefully than 502.
    pub head_height: Option<u64>,
    /// `head_height - indexer_height`. `null` when either side is
    /// unknown.
    pub head_lag_slots: Option<u64>,
}

/// Block summary, served at `GET /v1/blocks/{height}` and as each
/// element of the list at `GET /v1/blocks`.
///
/// Bech32m block hashes are flagged for a follow-up — ligate-api PR
/// #10 tracks the API-layer wrap. Today the chain emits
/// `block_hash` / `prev_hash` as hex `0x…` strings; the indexer
/// stores them verbatim. The wire format here echoes that until #10
/// lands a canonical conversion.
#[derive(Debug, Serialize)]
pub struct BlockResponse {
    /// Slot number (chain calls this `slot.number`; the api exposes
    /// it as `height` since that's the term partners reach for).
    pub height: u64,
    /// 32-byte slot hash, hex-prefixed `0x...` per chain output.
    pub hash: String,
    /// Hash of the previous slot. `null` only for the genesis slot.
    pub parent_hash: Option<String>,
    /// State root after slot execution. `null` if the chain didn't
    /// emit one (e.g. mock-DA dev mode that skips state-root checks).
    pub state_root: Option<String>,
    /// RFC3339 millisecond-precision UTC timestamp from the slot.
    pub timestamp: String,
    /// Number of transactions in the slot. Denormalised at ingest
    /// time so list views don't need a per-row join.
    pub tx_count: i32,
    /// Number of batches the chain emitted for this slot (mock-DA
    /// can emit > 1; production typically 1).
    pub batch_count: i32,
    /// Address that produced the block. `null` for v0 — the chain
    /// doesn't currently expose this in the slot JSON; reserved for
    /// when leader rotation lands (ligate-chain#82).
    pub proposer: Option<String>,
    /// Block size in bytes. `null` for v0 — same reason as
    /// `proposer`; reserved for when the chain emits it.
    pub size_bytes: Option<u64>,
}

/// One transaction, served at `GET /v1/txs/{hash}` and as each
/// element of the list at `GET /v1/txs`.
///
/// Per RFC 0002 §"Tx kinds": `kind` is a tagged-union discriminator;
/// `details` shape varies by `kind`. We surface `details` as a
/// pass-through JSON value rather than a typed enum, mirroring the
/// indexer's `transactions.details` JSONB column. Clients dispatch on
/// `kind` and decode `details` accordingly (see `ligate-js` for the
/// typed mapping).
///
/// `sender_pubkey`, `nonce`, and `fee_paid_nano` are nullable because
/// the chain elides borsh-encoded tx bodies from REST (see migration
/// 0003); the indexer fills `sender` from event payloads but leaves
/// the elided fields `null`. RFC 0002's "always present, null if
/// absent" rule applies — clients see the field name, just not data.
#[derive(Debug, Serialize)]
pub struct TxResponse {
    /// Transaction hash. Bech32m `ltx1...` on `ligate-chain@0ac7e5b`
    /// and later; hex `0x...` on older chain revs.
    pub hash: String,
    /// Slot height this tx landed in. Joins to `/v1/blocks/{height}`.
    pub block_height: u64,
    /// Block hash for the slot. May echo the slot's `lblk1...` form
    /// once chain emits that, or hex on legacy revs.
    pub block_hash: Option<String>,
    /// RFC3339 millisecond-precision timestamp of the slot. `null`
    /// only if the join to `slots` somehow missed (shouldn't happen
    /// for finalised slots; the indexer writes slots before txs).
    pub block_timestamp: Option<String>,
    /// Index of the tx within its block. Stable across reads; comes
    /// from the chain's per-slot ordering.
    pub position: i32,
    /// Sender address (`lig1...`) derived from `pubkey[..28]`. `null`
    /// if no recognised event in the tx exposed the sender (only
    /// `Bank/TokenTransferred` does today).
    pub sender: Option<String>,
    /// Sender pubkey (`lpk1...`). `null` in v0 — chain elides the
    /// pubkey from REST; reserved for when it becomes available.
    pub sender_pubkey: Option<String>,
    /// Account nonce. `null` in v0 — same reason as `sender_pubkey`.
    pub nonce: Option<i64>,
    /// Fee paid in nano-LGT (u128 as decimal string per RFC 0002).
    /// `null` in v0 — chain elides fee envelope.
    pub fee_paid_nano: Option<String>,
    /// Tagged-union discriminator. Values per RFC 0002 §"Tx kinds":
    /// `"transfer" | "register_attestor_set" | "register_schema" |
    /// "submit_attestation" | "unknown"`.
    pub kind: String,
    /// Per-`kind` payload. Shape pinned by RFC 0002. Pass-through
    /// from the indexer's `transactions.details` JSONB column.
    pub details: serde_json::Value,
    /// `"committed"` or `"reverted"`. Skipped txs aren't indexed.
    pub outcome: String,
    /// Free-form reverter reason when `outcome == "reverted"`. Chain
    /// doesn't currently emit this for tx-level reverts; `null` in v0.
    pub revert_reason: Option<String>,
}

/// Generic cursor pagination envelope, per RFC 0001.
///
/// Every list endpoint wraps its result rows in this shape. The
/// cursor in `pagination.next` is the base64url-encoded JSON shape
/// the endpoint's `Cursor` extractor expects on the next request
/// (e.g. `{"slot": 12345}` for `/v1/blocks`). Clients **MUST** treat
/// the cursor as opaque — its internal layout is per-endpoint and
/// reserved to change.
#[derive(Debug, Serialize)]
pub struct Page<T: Serialize> {
    /// The page of rows, descending by the endpoint's natural sort
    /// key (block height for `/v1/blocks`, etc.).
    pub data: Vec<T>,
    pub pagination: Pagination,
}

#[derive(Debug, Serialize)]
pub struct Pagination {
    /// Opaque cursor for the next page; `null` when this page is
    /// the last one. Pass back as `?before=...` on the next request.
    pub next: Option<String>,
    /// Resolved `limit` for this page (after server-side clamping
    /// against `MAX_PAGE_SIZE`).
    pub limit: u32,
}
