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

/// One schema, served at `GET /v1/schemas/{id}` and as each element
/// of the list at `GET /v1/schemas`.
///
/// Mirrors RFC 0002 §"Schema". `registered_at` is a nested
/// `{block_height, tx_hash, timestamp}` shape so partners can deep-link
/// to the registering tx without a separate lookup.
#[derive(Debug, Serialize)]
pub struct SchemaResponse {
    /// Bech32m `lsc1...` deterministic id.
    pub id: String,
    /// Schema name (e.g. `themisra.proof-of-prompt`).
    pub name: String,
    /// Monotonic version, scoped per (owner, name).
    pub version: u32,
    /// Owner address, bech32m `lig1...`.
    pub owner: String,
    /// Bound attestor set id, bech32m `las1...`.
    pub attestor_set_id: String,
    /// Fee-routing share in basis points (0..=10000).
    pub fee_routing_bps: u16,
    /// Destination for the routed share. `null` iff `bps == 0`.
    pub fee_routing_addr: Option<String>,
    /// SHA-256 of canonical schema-doc bytes. Format echoed from the
    /// chain event payload; typically hex with optional `0x` prefix.
    pub payload_shape_hash: String,
    /// Provenance of the registration tx.
    pub registered_at: RegisteredAtResponse,
    /// Denormalised count of attestations bound to this schema.
    /// Maintained at ingest time.
    pub attestation_count: u32,
}

/// One attestor set, served at `GET /v1/attestor-sets/{id}` and as
/// each element of `GET /v1/attestor-sets` (list endpoint isn't
/// shipped yet; reserved per RFC 0001's tracking ticket).
#[derive(Debug, Serialize)]
pub struct AttestorSetResponse {
    /// Bech32m `las1...` deterministic id.
    pub id: String,
    /// Member pubkeys, bech32m `lpk1...`. Sorted by raw byte order
    /// (matches the chain's canonicalisation rule used in
    /// `derive_id`).
    pub members: Vec<String>,
    /// M-of-N threshold (1..=members.len()).
    pub threshold: u8,
    /// Provenance of the registration tx.
    pub registered_at: RegisteredAtResponse,
    /// Denormalised count of schemas bound to this set. Maintained
    /// at ingest time.
    pub schema_count: u32,
}

/// Common `registered_at` sub-shape for [`SchemaResponse`] +
/// [`AttestorSetResponse`].
#[derive(Debug, Serialize)]
pub struct RegisteredAtResponse {
    /// Slot height of the registering tx.
    pub block_height: u64,
    /// Tx hash (bech32m `ltx1...` since `ligate-chain@0ac7e5b`).
    pub tx_hash: String,
    /// RFC3339 millisecond-precision UTC timestamp.
    pub timestamp: String,
}

/// Per-address summary, served at `GET /v1/addresses/{addr}`.
///
/// Balances come from the chain proxy at handler time (sov-bank's
/// `/v1/modules/bank/tokens/{token_id}/balances/{address}` is the
/// source of truth; the indexer doesn't mirror live balances because
/// they can change in-slot via a tx the indexer hasn't yet ingested).
/// Everything else comes from the indexer's denormalised
/// `address_summaries` table.
///
/// `schemas_owned_count` and `attestor_member_count` ship as `0` in
/// v0 because Phase D's schema / attestor-set ingest is blocked on
/// ligate-chain#295 (chain doesn't emit typed AttestationEvents
/// yet). Wire shape locked now so partners don't refactor when D
/// lands.
#[derive(Debug, Serialize)]
pub struct AddressSummaryResponse {
    /// Bech32m `lig1...` address, echoed back verbatim from the path
    /// parameter.
    pub address: String,
    /// Balances per token. Empty when the address has never received
    /// the gas token and no other tokens were transferred to it.
    /// Each entry's `amount_nano` is a u128 decimal string per RFC
    /// 0002.
    pub balances: Vec<TokenBalanceResponse>,
    /// Total txs the address participated in: `txs_sent + txs_received`.
    /// Computed at read time so partners don't have to add them
    /// themselves.
    pub tx_count: u64,
    /// First slot in which a tx involving this address landed.
    /// `null` for an address with no observed activity yet.
    pub first_seen: Option<SeenAtResponse>,
    /// Most recent slot in which a tx involving this address landed.
    /// `null` for an address with no observed activity yet.
    pub last_seen: Option<SeenAtResponse>,
    /// Number of registered schemas where this address is `owner`.
    /// Maintained by the (Phase D) schema ingest; `0` until that
    /// lands.
    pub schemas_owned_count: u32,
    /// Number of attestor sets where this address's pubkey is a
    /// member. Maintained by the (Phase D) attestor-set ingest; `0`
    /// until that lands.
    pub attestor_member_count: u32,
}

/// One token balance row inside [`AddressSummaryResponse.balances`].
#[derive(Debug, Serialize)]
pub struct TokenBalanceResponse {
    /// Bech32m `token_1...` form.
    pub token_id: String,
    /// u128 as decimal string per RFC 0002.
    pub amount_nano: String,
}

/// Provenance for first / last seen on an address. Sub-shape of
/// [`AddressSummaryResponse`].
#[derive(Debug, Serialize)]
pub struct SeenAtResponse {
    pub block_height: u64,
    pub timestamp: String,
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

/// One attestation, served at `GET /v1/attestations/{id}` and as each
/// element of `GET /v1/attestations` / `/v1/schemas/{id}/attestations`
/// / `/v1/attestor-sets/{id}/attestations`.
///
/// `id` is the compound `<schema_id>:<payload_hash>` form that
/// `/v1/attestations/{id}` accepts as a path parameter. Surfaced
/// separately from the constituent `schema_id` + `payload_hash`
/// fields so partners can pass `id` opaquely to deep-link routes
/// without re-composing it themselves.
#[derive(Debug, Serialize)]
pub struct AttestationResponse {
    /// Compound `<schema_id>:<payload_hash>` id (both bech32m).
    pub id: String,
    /// Bech32m `lsc1...` schema id this attestation is bound to.
    pub schema_id: String,
    /// Bech32m `lph1...` hash of the off-chain payload.
    pub payload_hash: String,
    /// Address that submitted the on-chain `SubmitAttestation` tx.
    /// NOT one of the attestors; the relayer.
    pub submitter: String,
    /// Pubkey of the submitter (32 bytes, bech32m `lpk1...`).
    /// `None` when the chain didn't emit it on the event payload
    /// (the `submitter` event field is `S::Address` only, not pubkey;
    /// migration 0004 relaxed `attestations.submitter_pubkey` to
    /// nullable per the indexer-side compromise). Partners who need
    /// the pubkey resolve via the `accounts` module's state at
    /// read time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitter_pubkey: Option<String>,
    /// Count of valid attestor signatures included. Chain enforces
    /// `>= schema.threshold`, so this is always populated and always
    /// at least 1.
    pub signature_count: u32,
    /// Provenance of the on-chain submission.
    pub submitted_at: RegisteredAtResponse,
}

/// `GET /v1/search?q=<string>` response.
///
/// `kind` discriminates which resource the query resolved to; the
/// remaining fields carry only what the explorer needs to redirect
/// (block height, tx hash, address, etc.). Full resource details
/// come from a follow-up call to the typed endpoint.
///
/// JSON shape (untagged enum is intentional — clients switch on
/// `kind` and read the resource-specific field):
///
/// ```json
/// { "kind": "block", "block_height": 4882 }
/// { "kind": "tx", "tx_hash": "ltx1..." }
/// { "kind": "address", "address": "lig1..." }
/// { "kind": "schema", "schema_id": "lsc1..." }
/// { "kind": "attestor_set", "attestor_set_id": "las1..." }
/// { "kind": "attestation", "schema_id": "lsc1...", "payload_hash": "lph1..." }
/// { "kind": "not_found", "query": "..." }
/// ```
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchResponse {
    Block {
        block_height: u64,
    },
    Tx {
        tx_hash: String,
    },
    Address {
        address: String,
    },
    Schema {
        schema_id: String,
    },
    AttestorSet {
        attestor_set_id: String,
    },
    Attestation {
        schema_id: String,
        payload_hash: String,
    },
    NotFound {
        query: String,
    },
}
