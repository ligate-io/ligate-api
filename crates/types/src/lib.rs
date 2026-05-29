//! Wire types for the Ligate Chain REST API.
//!
//! Mirrors the JSON shapes documented in the chain repo's
//! [`docs/protocol/rest-api.md`].
//!
//! These are deserialization-only types deliberately decoupled from
//! the protocol crates in `ligate-io/ligate-chain`. Indexers,
//! explorers, and third-party API clients can depend on this crate
//! without pulling the chain workspace plus the pinned Sovereign SDK
//! revision as transitive dependencies.
//!
//! [`docs/protocol/rest-api.md`]:
//!   https://github.com/ligate-io/ligate-chain/blob/main/docs/protocol/rest-api.md

#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bech32m HRP for chain addresses (`lig1...`).
pub const ADDRESS_HRP: &str = "lig";
/// Bech32m HRP for ed25519 public keys (`lpk1...`).
pub const PUBKEY_HRP: &str = "lpk";
/// Bech32m HRP for schema ids (`lsc1...`).
pub const SCHEMA_HRP: &str = "lsc";
/// Bech32m HRP for attestor set ids (`las1...`).
pub const ATTESTOR_SET_HRP: &str = "las";
/// Bech32m HRP for payload hashes (`lph1...`).
pub const PAYLOAD_HASH_HRP: &str = "lph";

// ============================================================================
// Rollup meta (`/v1/rollup/...`)
// ============================================================================

/// `GET /v1/rollup/info` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RollupInfo {
    /// Wallet/explorer-facing chain id, e.g. `ligate-devnet-2`.
    pub chain_id: String,
    /// Build-time fingerprint of the runtime, 64-char hex.
    pub chain_hash: String,
    /// Binary semver.
    pub version: String,
}

/// `GET /v1/rollup/sync-status` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SyncStatus {
    /// Whether the node is caught up to the DA layer head.
    pub synced: bool,
    /// DA height the node has processed up to.
    #[serde(default)]
    pub synced_da_height: Option<u64>,
    /// DA height the node is trying to reach.
    #[serde(default)]
    pub target_da_height: Option<u64>,
}

// ============================================================================
// Cluster topology (`/v1/cluster/nodes`)
// ============================================================================
//
// Two shapes:
//
//  - `ChainClusterTopology` / `ChainClusterNode` mirror the
//    chain's `/v1/cluster/nodes` shape. They include the per-node
//    VPC address; the api uses them to deserialize the chain
//    response before transforming into the public shape.
//  - `ClusterTopology` / `ClusterNode` are the public shape exposed
//    by `api.ligate.io/v1/cluster/nodes`. Private addresses are
//    stripped. `cluster_health` aggregates the topology into a
//    single string monitoring tools can branch on.

/// Public-facing topology response from `api.ligate.io/v1/cluster/nodes`.
/// Addresses are stripped; consumers see only node ids, roles, and
/// heartbeat ages. The leader's `acquired_at` is exposed so explorers
/// can render "leader since X minutes ago".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClusterTopology {
    /// Every node in the cluster, ordered leader-first then
    /// alphabetically. Replicas with stale heartbeats are NOT
    /// filtered out; consumers branch on `last_heartbeat_age_ms`.
    pub nodes: Vec<ClusterNode>,
    /// Node id of the current leader, or `None` during a failover
    /// window when no node holds the lock. Should clear within
    /// the cluster's `leader_timeout_millis` (default 500 ms).
    pub leader_node_id: Option<String>,
    /// Unix epoch milliseconds when the current leader first acquired
    /// the lock. `None` if no leader is held.
    pub leader_acquired_at_epoch_ms: Option<i64>,
    /// Unix epoch milliseconds when the api fetched this snapshot
    /// from the chain. Coupled with `Cache-Control` (default 5s) on
    /// the response so clients can detect they're seeing cached data.
    pub generated_at_epoch_ms: i64,
    /// One-word aggregate. `healthy` if a leader exists and every
    /// node's heartbeat is fresh; `degraded` if a leader exists but
    /// some node is stale; `leaderless` if no leader is held right
    /// now; `unknown` if the api couldn't reach the chain.
    pub cluster_health: ClusterHealth,
}

/// One node in the public topology response. Mirror of
/// `ChainClusterNode` without the private `address`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClusterNode {
    /// Stable per-VM identifier (e.g. `ligate-devnet-2-sequencer-2`).
    pub node_id: String,
    /// `true` exactly for the node holding the Postgres leader lock.
    pub is_leader: bool,
    /// Milliseconds since this node's last heartbeat. Fresh nodes
    /// sit at <100 ms (the cluster's heartbeat interval); values
    /// approaching `leader_timeout_millis` indicate trouble.
    pub last_heartbeat_age_ms: i64,
}

/// Cluster health aggregate. See [`ClusterTopology::cluster_health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    /// Leader exists and every node heartbeat is fresh.
    Healthy,
    /// Leader exists but at least one node is heartbeat-stale.
    Degraded,
    /// No node holds the leader lock right now. Failover in progress
    /// or election deadlock; should clear within ~12 seconds in a
    /// healthy DbElected cluster.
    Leaderless,
    /// The api couldn't reach the chain endpoint to fetch a topology.
    Unknown,
}

/// Internal shape used by the api to deserialize the chain's
/// response. Mirrors the chain's `ClusterTopology` exactly,
/// including the private `address` field. Never exposed publicly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainClusterTopology {
    /// Nodes in chain order (leader first, then alphabetical).
    pub nodes: Vec<ChainClusterNode>,
    /// Current leader's node id, or `None`.
    pub leader_node_id: Option<String>,
    /// Unix epoch milliseconds for the leader's `acquired_at`.
    pub leader_acquired_at_epoch_ms: Option<i64>,
    /// Snapshot time on the chain side.
    pub generated_at_epoch_ms: i64,
}

/// Internal node shape. Mirrors the chain's `ClusterNode`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainClusterNode {
    /// Stable per-VM identifier.
    pub node_id: String,
    /// Private VPC address (`host:port`). Stripped before the api
    /// returns the response publicly; kept here only so the api
    /// can deserialize the chain's response cleanly.
    pub address: String,
    /// `true` exactly for the node holding the Postgres leader lock.
    pub is_leader: bool,
    /// Heartbeat age in milliseconds.
    pub last_heartbeat_age_ms: i64,
}

// ============================================================================
// Attestation custom routes (`/v1/modules/attestation/...`)
// ============================================================================

/// `GET /v1/modules/attestation/schemas/{schemaId}` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaResponse {
    /// The schema record.
    pub schema: Schema,
}

/// One registered attestation schema.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Schema {
    /// Bech32m schema id (`lsc1...`).
    pub id: String,
    /// Bech32m owner address (`lig1...`).
    pub owner: String,
    /// Schema name.
    pub name: String,
    /// Schema version, monotonic per (owner, name).
    pub version: u32,
    /// Bech32m attestor set id (`las1...`) bound to this schema.
    pub attestor_set: String,
    /// Builder fee routing in basis points, 0 to 5000.
    pub fee_routing_bps: u16,
    /// Builder fee routing destination, present iff `fee_routing_bps > 0`.
    pub fee_routing_addr: Option<String>,
}

/// `GET /v1/modules/attestation/attestor-sets/{attestorSetId}` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestorSetResponse {
    /// The attestor set record.
    pub attestor_set: AttestorSet,
}

/// One registered attestor set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestorSet {
    /// Bech32m attestor set id (`las1...`).
    pub id: String,
    /// Member ed25519 pubkeys, each `lpk1...`.
    pub members: Vec<String>,
    /// M-of-N signature threshold.
    pub threshold: u32,
}

/// `GET /v1/modules/attestation/attestations/{schemaId}:{payloadHash}` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestationResponse {
    /// The attestation record.
    pub attestation: Attestation,
}

/// One submitted attestation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Attestation {
    /// Bech32m schema id (`lsc1...`).
    pub schema_id: String,
    /// Bech32m payload hash (`lph1...`).
    pub payload_hash: String,
    /// Bech32m submitter address (`lig1...`).
    pub submitter: String,
    /// Unix-seconds timestamp.
    pub timestamp: u64,
    /// One signature per attesting member.
    pub signatures: Vec<AttestorSignature>,
}

/// One attestor signature inside an [`Attestation`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestorSignature {
    /// Bech32m signer pubkey (`lpk1...`).
    pub pubkey: String,
    /// Hex-encoded signature bytes.
    pub sig: String,
}

// ============================================================================
// Bank custom routes (`/v1/modules/bank/...`)
// ============================================================================

/// `GET /v1/modules/bank/tokens/gas_token/balances/{address}` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BankBalanceResponse {
    /// Wrapped balance payload.
    pub data: BankBalance,
}

/// One holder's balance for one token.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BankBalance {
    /// Balance in base units (nanos for AVOW), serialized as a string
    /// because the chain returns `u64` as a JSON string to avoid loss
    /// of precision in JS clients.
    pub amount: String,
    /// Token id this balance is for.
    pub token_id: String,
}

// ============================================================================
// Ledger ("blocks", batches, transactions, events)
// ============================================================================
//
// The ledger surface is more shape-shifty between releases than the
// bespoke routes above. We retain the raw `serde_json::Value` payload
// alongside loosely-typed fields and let the indexer extract typed
// data progressively. This keeps the wire-types crate from breaking
// every time the SDK adds a field.

/// `GET /v1/ledger/slots/{slotId}` body. Mirrors the SDK's `Slot`
/// shape; treat fields as best-effort and the `raw` payload as
/// authoritative for anything not yet typed here.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SlotResponse {
    /// Slot height (rollup-side).
    pub number: u64,
    /// Hash of this slot.
    pub hash: String,
    /// Hash of the previous slot.
    #[serde(default)]
    pub prev_hash: Option<String>,
    /// Unix-seconds timestamp.
    #[serde(default)]
    pub timestamp: Option<u64>,
    /// State root after this slot.
    #[serde(default)]
    pub state_root: Option<String>,
    /// Number of batches in this slot.
    #[serde(default)]
    pub batch_count: Option<u64>,
    /// Number of transactions across all batches in this slot.
    #[serde(default)]
    pub tx_count: Option<u64>,
    /// Half-open range of batch numbers that landed in this slot.
    /// Present in current chain responses (e.g. `batch_range: {start:
    /// 7888, end: 7889}`); the indexer walks this to fetch each batch
    /// in turn.
    #[serde(default)]
    pub batch_range: Option<Uint64Range>,
    /// DA finality state of the slot. Chain emits one of:
    ///
    /// - `"pending"` — blob submitted to Celestia, awaiting N-block
    ///   confirmation (~12-15s on Mocha).
    /// - `"finalized"` — confirmation depth reached; data is
    ///   considered permanent.
    ///
    /// `None` on older chain revs that didn't surface this field.
    /// The indexer mirrors this value onto `slots.finality_status`
    /// and observes the `pending → finalized` transition wall-clock
    /// to populate `slots.finalized_at`.
    #[serde(default)]
    pub finality_status: Option<String>,
    /// Catch-all so unknown fields round-trip without loss.
    #[serde(flatten)]
    pub raw: std::collections::BTreeMap<String, Value>,
}

/// `GET /v1/ledger/txs/{txId}` body, lossy-typed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxResponse {
    /// Transaction hash.
    pub hash: String,
    /// Slot height the tx was included in.
    #[serde(default)]
    pub slot_number: Option<u64>,
    /// Inclusion status.
    #[serde(default)]
    pub status: Option<String>,
    /// Catch-all for fields the typed shape does not yet model.
    #[serde(flatten)]
    pub raw: std::collections::BTreeMap<String, Value>,
}

// ============================================================================
// LedgerTx + LedgerEvent (typed mirrors of sov-api-spec OpenAPI)
// ============================================================================
//
// Mirrors the `LedgerTx` / `LedgerEvent` schemas in the Sovereign SDK's
// `sov-api-spec/openapi-v3.yaml`. Used by the indexer to walk slots ->
// batches -> txs -> events while ingesting.
//
// Design choice: these types are STRICT (no `#[serde(flatten)] raw`)
// because they're the indexer's contract with the chain. If the chain
// adds a new required field, ingest fails loudly here and we update
// the type in the same PR. Better than silently dropping data into a
// catch-all and discovering it months later.
//
// One exception: `LedgerEvent.value` is intentionally `serde_json::Value`
// because each module's event payload has a different shape; the
// parser layer in `ligate-api-indexer` typed-decodes per-event-key.

/// `GET /v1/ledger/txs/{txId}` body, strict-typed.
///
/// Note `body.data` is empty in current chain releases — the chain
/// elides the tx body from JSON responses to avoid leaking unsigned
/// internals on a public RPC. Indexers extract semantic info from the
/// emitted [`LedgerEvent`]s, not from `body.data`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LedgerTx {
    /// Always `"tx"` (Sovereign SDK's tagged-type discriminator).
    #[serde(rename = "type")]
    pub r#type: String,

    /// Globally unique tx hash. Bech32m-encoded with HRP `ltx`
    /// (`ltx1...`) as of `ligate-chain` `0ac7e5b` and later;
    /// pre-bech32m chain revs returned lowercase hex with `0x` prefix.
    /// The indexer treats this opaquely: no format validation, just
    /// pass-through into Postgres.
    pub hash: String,

    /// Global tx index (NOT position-in-batch). Position-in-batch is
    /// derivable as `number - batch.tx_range.start`.
    pub number: u64,

    /// Range of [`LedgerEvent.number`]s emitted by this tx. `start..end`
    /// half-open. Empty range (`start == end`) for txs that emit no
    /// events.
    pub event_range: Uint64Range,

    /// Tx body wrapper. `data` is base64 of the borsh-encoded signed-tx
    /// bytes; `sequencing_data` is sequencer-supplied metadata. Both
    /// are usually empty / null in current chain releases.
    pub body: FullyBakedTx,

    /// Outcome of the tx. `result` is `"successful" | "reverted" | "skipped"`.
    pub receipt: TxReceipt,

    /// Inline events (only populated when the chain returns them via
    /// `?children=full` on a slot/batch fetch). Otherwise the indexer
    /// fetches events separately at `/v1/ledger/slots/{n}/events`.
    #[serde(default)]
    pub events: Vec<LedgerEvent>,

    /// Batch this tx landed in. Used to resolve `slot_number` via the
    /// `/v1/ledger/batches/{batch_number}` lookup.
    pub batch_number: u64,
}

/// Body wrapper inside [`LedgerTx`]. Both fields are usually empty
/// strings in current chain releases (the chain elides the body from
/// JSON for a public RPC).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FullyBakedTx {
    /// Base64 of the borsh-encoded signed-tx bytes. Empty string when
    /// the chain elides the body.
    pub data: String,
    /// Optional sequencer-supplied metadata in base64.
    #[serde(default)]
    pub sequencing_data: Option<String>,
}

/// Tx outcome wrapper. `result` is the discriminator partners switch on.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxReceipt {
    /// One of `"successful"`, `"reverted"`, `"skipped"`. RFC 0002 maps
    /// these to `outcome = "committed" | "reverted" | <not-indexed>`
    /// at the API layer.
    pub result: String,
    /// Generic per-result payload. For `"successful"`, contains
    /// `gas_used: [u64, u64]`. For other results, varies.
    pub data: Value,
}

/// Half-open `[start, end)` range of u64s. Used by `event_range`,
/// `tx_range`, `batch_range` throughout the ledger surface.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct Uint64Range {
    /// Inclusive start.
    pub start: u64,
    /// Exclusive end.
    pub end: u64,
}

/// `GET /v1/ledger/batches/{batchId}` body.
///
/// Each batch belongs to exactly one slot (via `slot_number`) and
/// covers a contiguous half-open range of transactions (`tx_range`).
/// The indexer walks `slot.batch_range`, fetches each batch, then
/// walks the batch's `tx_range` to fetch individual txs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LedgerBatch {
    /// Always `"batch"` (Sovereign SDK's tagged-type discriminator).
    #[serde(rename = "type")]
    pub r#type: String,
    /// Globally unique batch number.
    pub number: u64,
    /// Bech32m batch hash (`lba1...` on `ligate-chain@0ac7e5b` and
    /// later; hex `0x...` on older chain revs). Opaque to the
    /// indexer — passed verbatim into Postgres.
    pub hash: String,
    /// Slot this batch belongs to.
    pub slot_number: u64,
    /// Half-open range of tx numbers in this batch.
    pub tx_range: Uint64Range,
    /// Catch-all so unknown receipt / outcome fields round-trip
    /// without losing data the typed shape doesn't model yet.
    #[serde(flatten)]
    pub raw: std::collections::BTreeMap<String, Value>,
}

/// One typed event emitted during tx execution.
///
/// Module-emitted events are the indexer's source of truth for tx
/// semantics (since `LedgerTx.body.data` is empty in current chain
/// releases). The shape of `value` is per-`key`; the indexer parser
/// matches on `key` and decodes `value` accordingly.
///
/// Examples observed against localnet (chain `ligate-localnet`):
///
/// - `key = "Bank/TokenTransferred"`, `value = { token_transferred: { from, to, coins } }`
/// - `key = "Attestation/AttestorSetRegistered"`, `value = { attestor_set_registered: { ... } }` (TODO: confirm shape on next localnet test)
/// - `key = "Attestation/SchemaRegistered"`, `value = { schema_registered: { ... } }` (TODO)
/// - `key = "Attestation/AttestationSubmitted"`, `value = { attestation_submitted: { ... } }` (TODO)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LedgerEvent {
    /// Always `"event"`.
    #[serde(rename = "type")]
    pub r#type: String,

    /// Globally unique event index.
    pub number: u64,

    /// Event key in the form `"<Module>/<EventName>"`. The parser
    /// matches on this string to know which `value` shape to expect.
    pub key: String,

    /// Event payload. Per-event-key shape — the indexer's parser
    /// typed-decodes via `serde_json::from_value(...)` against
    /// per-event Rust structs.
    pub value: Value,

    /// Module reference. Always present; redundant with the prefix of
    /// `key` but exposed by the chain for convenience.
    pub module: ModuleRef,

    /// Tx hash this event was emitted from. Same format as
    /// [`LedgerTx::hash`]: bech32m `ltx1...` on current chain, hex
    /// `0x...` on pre-bech32m chain revs. Treated opaquely.
    pub tx_hash: String,
}

/// Module reference inside [`LedgerEvent`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleRef {
    /// Tagged-type discriminator. Always `"moduleRef"`.
    #[serde(rename = "type", default)]
    pub r#type: String,
    /// Module name, e.g. `"Bank"` or `"Attestation"`.
    pub name: String,
}

// ---- Per-event payload shapes (typed via serde_json::from_value) ----------
//
// One typed payload per `(module, event_name)` pair we ingest. The
// indexer's parser switches on `LedgerEvent.key`, then deserialises
// `LedgerEvent.value` into the matching struct.

/// Payload of `Bank/TokenTransferred`.
///
/// Wire shape (captured from localnet tx
/// `ltx19zwttsdksue0ef4fan7lnfhcjdq9lq8d592hjpcc30gh5c77ytzqvjmjm4`
/// against chain `ligate-localnet`; pre-bech32m chain revs returned
/// the same payload byte-identical, just with `0x...` hex hashes):
///
/// ```json
/// {
///   "token_transferred": {
///     "from": { "user": "lig1..." },
///     "to": { "user": "lig1..." },
///     "coins": { "amount": "1000000000", "token_id": "token_1..." }
///   }
/// }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BankTokenTransferredEvent {
    /// Inner wrapper (Sovereign SDK's tagged-enum serialisation
    /// produces `{ <variant_name>: <fields> }`).
    pub token_transferred: BankTransferDetails,
}

/// Inner fields of a `Bank/TokenTransferred` event.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BankTransferDetails {
    /// Sender, wrapped in the chain's `MultiAddress::User(addr)` shape.
    pub from: MultiAddress,
    /// Recipient.
    pub to: MultiAddress,
    /// Coins moved.
    pub coins: Coins,
}

/// `MultiAddress` wrapper from the chain. The `user` variant is the
/// only one observed on the public surface; module-internal variants
/// are unwrapped before they hit events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MultiAddress {
    /// Bech32m `lig1...` address.
    pub user: String,
}

// ---- Attestation module event payloads -------------------------------------
//
// Each variant of `attestation::AttestationEvent<S>` serialises via
// serde's default externally-tagged enum encoding: `{<snake_case
// variant>: {<fields>}}`. The three structs below mirror the chain's
// `crates/modules/attestation/src/lib.rs::AttestationEvent` payload
// shapes for indexing. Chain-side spec lives in ligate-chain PR #297.

/// Payload of `AttestationModule/AttestorSetRegistered`.
///
/// **Wire format note.** The chain's emitted event has the variant
/// name in PascalCase as the outer JSON key (serde's default
/// externally-tagged enum encoding for the chain's
/// `AttestationEvent::AttestorSetRegistered { ... }` variant). The
/// `#[serde(rename)]` here decouples the Rust field name from the
/// wire name so the descriptive `attestor_set_registered` accessor
/// pattern in the parser stays readable.
///
/// The address fields inside (`registered_by`, etc.) are emitted as
/// **raw bech32m strings**, NOT the `{"user": "lig1..."}` wrapper
/// the bank module still uses. The two modules currently disagree on
/// `MultiAddress` serialization; we match each event's actual shape
/// rather than imposing one wrapper. If the chain unifies them later,
/// this is where to track the change.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestationAttestorSetRegisteredEvent {
    /// Inner externally-tagged variant. Wire name is the PascalCase
    /// variant identifier (`AttestorSetRegistered`); Rust field name
    /// is the descriptive snake_case form for ergonomic access in the
    /// parser.
    #[serde(rename = "AttestorSetRegistered")]
    pub attestor_set_registered: AttestorSetRegisteredDetails,
}

/// Inner fields of `AttestationModule/AttestorSetRegistered`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestorSetRegisteredDetails {
    /// Bech32m `las1...` deterministic id.
    pub attestor_set_id: String,
    /// Member pubkeys (bech32m `lpk1...`), sorted post-canonicalisation.
    pub members: Vec<String>,
    /// M-of-N threshold.
    pub threshold: u8,
    /// Address that paid the registration fee. Raw bech32m `lig1...`
    /// string (NOT wrapped in `{"user": ...}` — see top-level
    /// docstring for the chain-side rationale).
    pub registered_by: String,
}

/// Payload of `AttestationModule/SchemaRegistered`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestationSchemaRegisteredEvent {
    /// Inner externally-tagged variant. See
    /// [`AttestationAttestorSetRegisteredEvent`] for the wire-vs-Rust
    /// naming convention.
    #[serde(rename = "SchemaRegistered")]
    pub schema_registered: SchemaRegisteredDetails,
}

/// Inner fields of `AttestationModule/SchemaRegistered`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaRegisteredDetails {
    /// Bech32m `lsc1...` deterministic id.
    pub schema_id: String,
    /// Schema name (e.g. `themisra.proof-of-prompt`).
    pub name: String,
    /// Schema version (monotonic per name+owner).
    pub version: u32,
    /// Owner address — receives schema-routed fees. Raw bech32m string.
    pub owner: String,
    /// Bound attestor set id (bech32m `las1...`).
    pub attestor_set_id: String,
    /// Fee-routing share in basis points (0..=cap).
    pub fee_routing_bps: u16,
    /// Destination address for the routed share. `None` iff
    /// `fee_routing_bps == 0`. Raw bech32m string.
    #[serde(default)]
    pub fee_routing_addr: Option<String>,
    /// SHA-256 of the canonical schema-doc bytes. Chain serialises
    /// the `[u8; 32]` as a hex string (with or without `0x` prefix
    /// depending on the chain rev). Kept as `serde_json::Value` so
    /// future chain encodings (e.g. bech32m wrap) don't break ingest;
    /// the indexer stringifies and stores verbatim.
    pub payload_shape_hash: Value,
}

/// Payload of `AttestationModule/AttestationSubmitted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestationAttestationSubmittedEvent {
    /// Inner externally-tagged variant. See
    /// [`AttestationAttestorSetRegisteredEvent`] for the wire-vs-Rust
    /// naming convention.
    #[serde(rename = "AttestationSubmitted")]
    pub attestation_submitted: AttestationSubmittedDetails,
}

/// Inner fields of `AttestationModule/AttestationSubmitted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttestationSubmittedDetails {
    /// Bech32m `lsc1...` schema id.
    pub schema_id: String,
    /// Bech32m `lph1...` payload hash.
    pub payload_hash: String,
    /// Submitter address (paid the attestation fee). Raw bech32m string.
    pub submitter: String,
    /// Number of attestor signatures included.
    pub signature_count: u32,
}

/// `(amount, token_id)` pair as emitted by the bank module.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Coins {
    /// Amount as a decimal string (chain uses u128, JS-compat).
    pub amount: String,
    /// Bech32m token id (`token_1...`).
    pub token_id: String,
}

// ============================================================================
// Bounty module (`/v1/modules/bounty/...`)
// ============================================================================

/// `GET /v1/modules/bounty/bounties/{id}` body.
///
/// The indexer hydrates this after seeing any `Bounty/*` event, since
/// those events are thin (id + amounts only) and don't carry the full
/// bounty record. The inner [`BountyRecord`] is the authoritative
/// source for the static fields (board schema, per-attestation payout,
/// acceptance predicate, expiry, dispute window) AND the live `status`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyResponse {
    /// The bounty record.
    pub bounty: BountyRecord,
}

/// One bounty record from chain state. Mirrors the bounty module's
/// on-chain `Bounty` struct (chain#519, ligate-chain v0.4.0+) closely
/// enough for the indexer to populate the `bounties` table.
///
/// Amounts are u128 decimal strings (JS-compat, same convention as
/// the bank module's [`Coins::amount`]). Addresses + ids are raw
/// bech32m strings (NOT the bank module's `{"user": "..."}` wrapper),
/// matching the bounty module's event serialisation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyRecord {
    /// Poster address (bech32m `lig1...`). Receives escrow refunds on
    /// cancel and rejected-dispute bond payouts.
    pub poster: String,
    /// Bech32m `lsc1...` of the bounty board schema this bounty
    /// composes against.
    pub board_schema_id: String,
    /// Original pool size at PostBounty time, in AVOW nanos. u128
    /// decimal string.
    pub pool: String,
    /// AVOW paid out per accepted claim, in nanos. u128 decimal string.
    pub per_attestation: String,
    /// Acceptance predicate as compact JSON. Mirrors the chain's
    /// `AcceptancePredicate` enum; the indexer stores it verbatim in
    /// the `bounties.acceptance` JSONB column.
    pub acceptance: Value,
    /// DA-layer block height the bounty expires at.
    pub expiry_da_height: u64,
    /// Dispute window in chain blocks.
    pub dispute_window_blocks: u32,
    /// Lifecycle state. Chain emits PascalCase
    /// (`"Open"`/`"Exhausted"`/`"Expired"`/`"Cancelled"`/`"Finalised"`);
    /// the indexer maps to the lowercase `bounties.status` CHECK enum.
    pub status: String,
}

// ---- Bounty module event payloads ------------------------------------------
//
// Each variant of the chain's `BountyEvent` serialises via serde's
// default externally-tagged enum encoding: `{<PascalCaseVariant>:
// {<fields>}}`. The structs below mirror the chain's bounty-module
// event shapes for indexing, using the same `#[serde(rename)]` +
// descriptive-snake_case-field convention as the attestation events
// above. Addresses + ids are raw bech32m strings; amounts are u128
// decimal strings.
//
// The events are intentionally THIN: they carry only the bounty id
// (plus per-event extras like attestation id / payout / refund
// amount). The indexer re-hydrates the full record via
// [`BountyResponse`] on every event, so these payloads only need the
// fields that drive per-event deltas (claim accounting, escrow-zeroing).

/// Payload of `Bounty/BountyPosted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyPostedEvent {
    /// Inner externally-tagged variant. Wire name is the PascalCase
    /// variant identifier; Rust field name is the descriptive
    /// snake_case form (see attestation events for the convention).
    #[serde(rename = "BountyPosted")]
    pub bounty_posted: BountyPostedDetails,
}

/// Inner fields of `Bounty/BountyPosted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyPostedDetails {
    /// Bech32m `lbt1...` deterministic bounty id.
    pub bounty_id: String,
    /// Poster address (raw bech32m `lig1...`).
    pub poster: String,
    /// Initial escrow pool, u128 decimal string.
    pub pool: String,
}

/// Payload of `Bounty/BountyClaimed`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyClaimedEvent {
    /// Inner externally-tagged variant. See [`BountyPostedEvent`] for
    /// the wire-vs-Rust naming convention.
    #[serde(rename = "BountyClaimed")]
    pub bounty_claimed: BountyClaimedDetails,
}

/// Inner fields of `Bounty/BountyClaimed`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyClaimedDetails {
    /// Bech32m `lbt1...` bounty id.
    pub bounty_id: String,
    /// Bech32m `lat1...` attestation id that satisfied the claim.
    pub attestation_id: String,
    /// Payout for this claim, u128 decimal string.
    pub payout: String,
    /// Attester address paid out (raw bech32m `lig1...`).
    pub attester: String,
}

/// Payload of `Bounty/BountyDisputed`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyDisputedEvent {
    /// Inner externally-tagged variant. See [`BountyPostedEvent`].
    #[serde(rename = "BountyDisputed")]
    pub bounty_disputed: BountyDisputedDetails,
}

/// Inner fields of `Bounty/BountyDisputed`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyDisputedDetails {
    /// Bech32m `lbt1...` bounty id.
    pub bounty_id: String,
    /// Bech32m `lat1...` attestation id under dispute.
    pub attestation_id: String,
    /// Disputer address (raw bech32m `lig1...`).
    pub disputer: String,
    /// Dispute bond posted, u128 decimal string.
    pub bond: String,
}

/// Payload of `Bounty/DisputeResolved`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DisputeResolvedEvent {
    /// Inner externally-tagged variant. See [`BountyPostedEvent`].
    #[serde(rename = "DisputeResolved")]
    pub dispute_resolved: DisputeResolvedDetails,
}

/// Inner fields of `Bounty/DisputeResolved`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DisputeResolvedDetails {
    /// Bech32m `lbt1...` bounty id.
    pub bounty_id: String,
    /// Bech32m `lat1...` attestation id the dispute targeted.
    pub attestation_id: String,
    /// Resolution decision: `"Accept"` or `"Reject"`.
    pub decision: String,
    /// Address that received the bond (raw bech32m `lig1...`).
    pub bond_recipient: String,
}

/// Payload of `Bounty/BountyExpired`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyExpiredEvent {
    /// Inner externally-tagged variant. See [`BountyPostedEvent`].
    #[serde(rename = "BountyExpired")]
    pub bounty_expired: BountyExpiredDetails,
}

/// Inner fields of `Bounty/BountyExpired`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyExpiredDetails {
    /// Bech32m `lbt1...` bounty id.
    pub bounty_id: String,
    /// Amount refunded to the poster, u128 decimal string.
    pub refunded_to_poster: String,
}

/// Payload of `Bounty/BountyFinalised`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyFinalisedEvent {
    /// Inner externally-tagged variant. See [`BountyPostedEvent`].
    #[serde(rename = "BountyFinalised")]
    pub bounty_finalised: BountyFinalisedDetails,
}

/// Inner fields of `Bounty/BountyFinalised`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BountyFinalisedDetails {
    /// Bech32m `lbt1...` bounty id.
    pub bounty_id: String,
    /// Residual escrow swept back to the poster, u128 decimal string.
    pub swept_to_poster: String,
}

// ============================================================================
// Contract module (`/v1/modules/contracts/...`)
// ============================================================================

/// `GET /v1/modules/contracts/contracts/{id}` body.
///
/// The indexer hydrates this after seeing any `Contracts/*` event,
/// since those events are thin (id + addresses/amounts only) and don't
/// carry the full contract record. The inner [`ContractRecord`] is the
/// authoritative source for the static fields (arbiter, criteria doc
/// hash, pool, expiry, dispute window, arbiter fee) AND the live
/// `status`.
///
/// **Module struct name.** The contract module's struct is `Contracts`
/// (plural — see ligate-chain `crates/modules/contract/src/lib.rs`), so
/// the SDK-derived event-key prefix is `Contracts/` (NOT `Contract/`)
/// and the REST custom route lives under `/modules/contracts/...`. This
/// envelope mirrors the chain's `ContractResponse { contract }` shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractResponse {
    /// The contract record.
    pub contract: ContractRecord,
}

/// One contract record from chain state. Mirrors the contract module's
/// on-chain `ContractState` struct (chain contract primitive,
/// ligate-chain v0.4.0+) closely enough for the indexer to populate
/// the `contracts` table.
///
/// Amounts are u128 decimal strings (JS-compat, same convention as the
/// bank module's [`Coins::amount`] and the bounty module's
/// [`BountyRecord`]). Addresses are raw bech32m `lig1...` strings (NOT
/// the bank module's `{"user": "..."}` wrapper), matching the contract
/// module's event serialisation. `criteria_doc_hash` is hedged as a
/// `serde_json::Value` (same rationale as `payload_shape_hash`): the
/// chain serialises the `[u8; 32]` as a hex string today, but the
/// indexer stringifies and stores verbatim so a future encoding switch
/// (e.g. bech32m wrap) doesn't break ingest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractRecord {
    /// Poster address (buyer), bech32m `lig1...`. Receives refunds on
    /// cancel and bond payouts on rejected dispute resolutions.
    pub poster: String,
    /// Arbiter address named at post time, bech32m `lig1...`.
    /// Authorised to resolve disputes on this contract.
    pub arbiter: String,
    /// 32-byte content hash of the off-chain criteria document. Chain
    /// serialises the `[u8; 32]` as a hex string (with or without `0x`
    /// prefix depending on the chain rev). Kept as `serde_json::Value`
    /// so future chain encodings don't break ingest; the indexer
    /// stringifies and stores verbatim. Same hedge as
    /// [`SchemaRegisteredDetails::payload_shape_hash`].
    pub criteria_doc_hash: Value,
    /// Total `AVOW` originally escrowed, in nanos. u128 decimal string.
    pub pool: String,
    /// DA-layer block height the contract expires at.
    pub expiry_da_height: u64,
    /// Window in chain blocks the poster has to accept-or-reject a
    /// delivery before it auto-accepts.
    pub dispute_window_blocks: u32,
    /// Arbiter fee in basis points (paid only if the arbiter resolves a
    /// dispute). Default 500 bps (5%) on chain.
    pub arbiter_fee_bps: u16,
    /// Lifecycle state. Chain emits PascalCase (`"Open"`/`"Committed"`/
    /// `"Delivered"`/`"Accepted"`/`"Rejected"`/`"Disputed"`/
    /// `"Cancelled"`/`"Expired"`); the indexer maps to the lowercase
    /// `contracts.status` CHECK enum.
    pub status: String,
}

// ---- Contract module event payloads ----------------------------------------
//
// Each variant of the chain's contract-module `Event` serialises via
// serde's default externally-tagged enum encoding: `{<PascalCaseVariant>:
// {<fields>}}`. The structs below mirror the chain's contract-module
// event shapes for indexing, using the same `#[serde(rename)]` +
// descriptive-snake_case-field convention as the bounty/attestation
// events above. Addresses + ids are raw bech32m strings; amounts are
// u128 decimal strings.
//
// **Event-key prefix.** The contract module's struct is `Contracts`
// (plural), so the SDK-derived event keys are `Contracts/ContractPosted`,
// `Contracts/WorkerCommitted`, etc. (the SDK builds the key as
// `format!("{module_struct_ident}/{variant}")`). This is the single
// most error-prone spot in mirroring the bounty work — bounty's struct
// is `Bounty`, hence `Bounty/...`, but contract's is `Contracts`.
//
// The events are intentionally THIN: they carry only the contract id
// (plus per-event extras like worker / payout / refund amount). The
// indexer re-hydrates the full record via [`ContractResponse`] on every
// event, so these payloads only need the fields that drive per-event
// deltas (escrow-zeroing on terminal states).

/// Payload of `Contracts/ContractPosted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractPostedEvent {
    /// Inner externally-tagged variant. Wire name is the PascalCase
    /// variant identifier; Rust field name is the descriptive
    /// snake_case form (see bounty events for the convention).
    #[serde(rename = "ContractPosted")]
    pub contract_posted: ContractPostedDetails,
}

/// Inner fields of `Contracts/ContractPosted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractPostedDetails {
    /// Bech32m `lct1...` deterministic contract id.
    pub contract_id: String,
    /// Poster address (raw bech32m `lig1...`).
    pub poster: String,
    /// Arbiter named at post time (raw bech32m `lig1...`).
    pub arbiter: String,
    /// Initial escrow pool, u128 decimal string.
    pub pool: String,
}

/// Payload of `Contracts/WorkerCommitted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerCommittedEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "WorkerCommitted")]
    pub worker_committed: WorkerCommittedDetails,
}

/// Inner fields of `Contracts/WorkerCommitted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerCommittedDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Worker address (raw bech32m `lig1...`).
    pub worker: String,
    /// SHA-256 commitment to the deliverable. Chain serialises the
    /// `[u8; 32]` as a hex string; kept as `Value` for the same
    /// forward-compat reason as [`ContractRecord::criteria_doc_hash`].
    #[serde(default)]
    pub commit_hash: Value,
    /// Bond locked, u128 decimal string.
    pub bond: String,
}

/// Payload of `Contracts/ContractDelivered`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractDeliveredEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "ContractDelivered")]
    pub contract_delivered: ContractDeliveredDetails,
}

/// Inner fields of `Contracts/ContractDelivered`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractDeliveredDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Worker who delivered (raw bech32m `lig1...`).
    pub worker: String,
    /// Bech32m `lat1...` attestation id pointing at the deliverable's
    /// proof of work.
    pub deliverable_attestation_id: String,
}

/// Payload of `Contracts/DeliveryAccepted`. Emitted both on the
/// poster's explicit `AcceptDelivery` and on the permissionless
/// `FinalizeDelivery` auto-accept sweep (the chain emits the same
/// `DeliveryAccepted` event for both — the indexer treats them
/// identically: payout settled, escrow drained).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryAcceptedEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "DeliveryAccepted")]
    pub delivery_accepted: DeliveryAcceptedDetails,
}

/// Inner fields of `Contracts/DeliveryAccepted`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryAcceptedDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Worker who got paid (raw bech32m `lig1...`).
    pub worker: String,
    /// Payout amount, u128 decimal string.
    pub payout: String,
}

/// Payload of `Contracts/DeliveryRejected`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryRejectedEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "DeliveryRejected")]
    pub delivery_rejected: DeliveryRejectedDetails,
}

/// Inner fields of `Contracts/DeliveryRejected`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryRejectedDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Worker whose delivery was rejected (raw bech32m `lig1...`).
    pub worker: String,
    /// Dispute ground. Chain emits one of `"CriteriaMismatch"` /
    /// `"MalformedDelivery"` / `"ExpiredAtDelivery"` / `"Other"`. Kept
    /// as a string pass-through; the indexer doesn't branch on it.
    pub reason: String,
}

/// Payload of `Contracts/ContractDisputeResolved`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractDisputeResolvedEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "ContractDisputeResolved")]
    pub contract_dispute_resolved: ContractDisputeResolvedDetails,
}

/// Inner fields of `Contracts/ContractDisputeResolved`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractDisputeResolvedDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Resolution decision: `"AcceptDelivery"` or `"RejectDelivery"`.
    pub decision: String,
    /// Address that received the pool (raw bech32m `lig1...`).
    pub winner: String,
}

/// Payload of `Contracts/ContractCancelled`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractCancelledEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "ContractCancelled")]
    pub contract_cancelled: ContractCancelledDetails,
}

/// Inner fields of `Contracts/ContractCancelled`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractCancelledDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Amount refunded to the poster, u128 decimal string.
    pub refunded_to_poster: String,
}

/// Payload of `Contracts/ContractExpired`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractExpiredEvent {
    /// Inner externally-tagged variant. See [`ContractPostedEvent`].
    #[serde(rename = "ContractExpired")]
    pub contract_expired: ContractExpiredDetails,
}

/// Inner fields of `Contracts/ContractExpired`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractExpiredDetails {
    /// Bech32m `lct1...` contract id.
    pub contract_id: String,
    /// Amount refunded to the poster, u128 decimal string.
    pub refunded_to_poster: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_info_round_trip() {
        let body = r#"{"chain_id":"ligate-devnet-2","chain_hash":"abcd","version":"0.0.1"}"#;
        let parsed: RollupInfo = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.chain_id, "ligate-devnet-2");
        assert_eq!(parsed.chain_hash, "abcd");
        assert_eq!(parsed.version, "0.0.1");
    }

    #[test]
    fn schema_response_round_trip() {
        let body = r#"{
            "schema": {
                "id": "lsc1xyz",
                "owner": "lig1abc",
                "name": "themisra.proof-of-prompt",
                "version": 1,
                "attestor_set": "las1def",
                "fee_routing_bps": 0,
                "fee_routing_addr": null
            }
        }"#;
        let parsed: SchemaResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.schema.id, "lsc1xyz");
        assert_eq!(parsed.schema.fee_routing_bps, 0);
        assert!(parsed.schema.fee_routing_addr.is_none());
    }

    #[test]
    fn attestor_set_response_round_trip() {
        let body = r#"{
            "attestor_set": {
                "id": "las1abc",
                "members": ["lpk1one", "lpk1two", "lpk1three"],
                "threshold": 2
            }
        }"#;
        let parsed: AttestorSetResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.attestor_set.members.len(), 3);
        assert_eq!(parsed.attestor_set.threshold, 2);
    }

    #[test]
    fn bounty_response_round_trip() {
        // Chain emits PascalCase `status` and u128 amounts as strings;
        // `acceptance` is a pass-through JSON object.
        let body = r#"{
            "bounty": {
                "poster": "lig1poster",
                "board_schema_id": "lsc1board",
                "pool": "5000000000",
                "per_attestation": "1000000000",
                "acceptance": {"Any": {}},
                "expiry_da_height": 123456,
                "dispute_window_blocks": 100,
                "status": "Open"
            }
        }"#;
        let parsed: BountyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.bounty.poster, "lig1poster");
        assert_eq!(parsed.bounty.board_schema_id, "lsc1board");
        assert_eq!(parsed.bounty.pool, "5000000000");
        assert_eq!(parsed.bounty.per_attestation, "1000000000");
        assert_eq!(parsed.bounty.expiry_da_height, 123456);
        assert_eq!(parsed.bounty.dispute_window_blocks, 100);
        assert_eq!(parsed.bounty.status, "Open");
        assert_eq!(parsed.bounty.acceptance["Any"], serde_json::json!({}));
    }

    #[test]
    fn bounty_event_payloads_round_trip() {
        // Externally-tagged enum encoding: PascalCase variant key, raw
        // bech32m addresses (no `{"user": ...}` wrapper), u128 strings.
        let posted: BountyPostedEvent = serde_json::from_value(serde_json::json!({
            "BountyPosted": {
                "bounty_id": "lbt1abc",
                "poster": "lig1poster",
                "pool": "5000000000"
            }
        }))
        .unwrap();
        assert_eq!(posted.bounty_posted.bounty_id, "lbt1abc");
        assert_eq!(posted.bounty_posted.pool, "5000000000");

        let claimed: BountyClaimedEvent = serde_json::from_value(serde_json::json!({
            "BountyClaimed": {
                "bounty_id": "lbt1abc",
                "attestation_id": "lat1xyz",
                "payout": "1000000000",
                "attester": "lig1attester"
            }
        }))
        .unwrap();
        assert_eq!(claimed.bounty_claimed.bounty_id, "lbt1abc");
        assert_eq!(claimed.bounty_claimed.payout, "1000000000");
        assert_eq!(claimed.bounty_claimed.attester, "lig1attester");

        let expired: BountyExpiredEvent = serde_json::from_value(serde_json::json!({
            "BountyExpired": { "bounty_id": "lbt1abc", "refunded_to_poster": "4000000000" }
        }))
        .unwrap();
        assert_eq!(expired.bounty_expired.refunded_to_poster, "4000000000");

        let finalised: BountyFinalisedEvent = serde_json::from_value(serde_json::json!({
            "BountyFinalised": { "bounty_id": "lbt1abc", "swept_to_poster": "0" }
        }))
        .unwrap();
        assert_eq!(finalised.bounty_finalised.swept_to_poster, "0");
    }

    #[test]
    fn contract_response_round_trip() {
        // Chain emits PascalCase `status`, u128 amounts as strings,
        // raw bech32m addresses, and `criteria_doc_hash` as a hex
        // string (hedged as a JSON Value on our side).
        let body = r#"{
            "contract": {
                "poster": "lig1poster",
                "arbiter": "lig1arbiter",
                "criteria_doc_hash": "0xdeadbeef",
                "pool": "5000000000",
                "expiry_da_height": 123456,
                "dispute_window_blocks": 100,
                "arbiter_fee_bps": 500,
                "status": "Open"
            }
        }"#;
        let parsed: ContractResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.contract.poster, "lig1poster");
        assert_eq!(parsed.contract.arbiter, "lig1arbiter");
        assert_eq!(parsed.contract.criteria_doc_hash, "0xdeadbeef");
        assert_eq!(parsed.contract.pool, "5000000000");
        assert_eq!(parsed.contract.expiry_da_height, 123456);
        assert_eq!(parsed.contract.dispute_window_blocks, 100);
        assert_eq!(parsed.contract.arbiter_fee_bps, 500);
        assert_eq!(parsed.contract.status, "Open");
    }

    #[test]
    fn contract_event_payloads_round_trip() {
        // Externally-tagged enum encoding: PascalCase variant key, raw
        // bech32m addresses (no `{"user": ...}` wrapper), u128 strings.
        let posted: ContractPostedEvent = serde_json::from_value(serde_json::json!({
            "ContractPosted": {
                "contract_id": "lct1abc",
                "poster": "lig1poster",
                "arbiter": "lig1arbiter",
                "pool": "5000000000"
            }
        }))
        .unwrap();
        assert_eq!(posted.contract_posted.contract_id, "lct1abc");
        assert_eq!(posted.contract_posted.arbiter, "lig1arbiter");
        assert_eq!(posted.contract_posted.pool, "5000000000");

        let committed: WorkerCommittedEvent = serde_json::from_value(serde_json::json!({
            "WorkerCommitted": {
                "contract_id": "lct1abc",
                "worker": "lig1worker",
                "commit_hash": "0xc0ffee",
                "bond": "250000000"
            }
        }))
        .unwrap();
        assert_eq!(committed.worker_committed.worker, "lig1worker");
        assert_eq!(committed.worker_committed.bond, "250000000");

        let delivered: ContractDeliveredEvent = serde_json::from_value(serde_json::json!({
            "ContractDelivered": {
                "contract_id": "lct1abc",
                "worker": "lig1worker",
                "deliverable_attestation_id": "lat1xyz"
            }
        }))
        .unwrap();
        assert_eq!(
            delivered.contract_delivered.deliverable_attestation_id,
            "lat1xyz"
        );

        let accepted: DeliveryAcceptedEvent = serde_json::from_value(serde_json::json!({
            "DeliveryAccepted": {
                "contract_id": "lct1abc",
                "worker": "lig1worker",
                "payout": "5000000000"
            }
        }))
        .unwrap();
        assert_eq!(accepted.delivery_accepted.payout, "5000000000");

        let rejected: DeliveryRejectedEvent = serde_json::from_value(serde_json::json!({
            "DeliveryRejected": {
                "contract_id": "lct1abc",
                "worker": "lig1worker",
                "reason": "CriteriaMismatch"
            }
        }))
        .unwrap();
        assert_eq!(rejected.delivery_rejected.reason, "CriteriaMismatch");

        let resolved: ContractDisputeResolvedEvent = serde_json::from_value(serde_json::json!({
            "ContractDisputeResolved": {
                "contract_id": "lct1abc",
                "decision": "AcceptDelivery",
                "winner": "lig1worker"
            }
        }))
        .unwrap();
        assert_eq!(
            resolved.contract_dispute_resolved.decision,
            "AcceptDelivery"
        );
        assert_eq!(resolved.contract_dispute_resolved.winner, "lig1worker");

        let cancelled: ContractCancelledEvent = serde_json::from_value(serde_json::json!({
            "ContractCancelled": { "contract_id": "lct1abc", "refunded_to_poster": "5000000000" }
        }))
        .unwrap();
        assert_eq!(
            cancelled.contract_cancelled.refunded_to_poster,
            "5000000000"
        );

        let expired: ContractExpiredEvent = serde_json::from_value(serde_json::json!({
            "ContractExpired": { "contract_id": "lct1abc", "refunded_to_poster": "5000000000" }
        }))
        .unwrap();
        assert_eq!(expired.contract_expired.refunded_to_poster, "5000000000");
    }

    #[test]
    fn slot_response_preserves_unknown_fields() {
        let body = r#"{
            "number": 42,
            "hash": "lblk1abc",
            "prev_hash": "lblk1def",
            "timestamp": 1700000000,
            "future_field": "future_value"
        }"#;
        let parsed: SlotResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.number, 42);
        assert_eq!(parsed.raw.get("future_field").unwrap(), "future_value");
    }
}

// ============================================================================
// Bounty module events (`/v1/ledger/events`)
// ============================================================================

/// `Bounty/BountyPosted` event wrapper emitted by the bounty module.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyBountyPostedEvent {
    /// Inner event payload.
    pub bounty_posted: BountyPostedPayload,
}

/// Inner `BountyPosted` payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyPostedPayload {
    /// Chain bounty id.
    pub bounty_id: String,
    /// Poster address (`lig1...`).
    pub poster: String,
    /// Escrow pool in nanos, encoded as decimal string to preserve precision.
    pub pool: String,
}

/// `Bounty/BountyClaimed` event wrapper emitted by the bounty module.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyBountyClaimedEvent {
    /// Inner event payload.
    pub bounty_claimed: BountyClaimedPayload,
}

/// Inner `BountyClaimed` payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyClaimedPayload {
    /// Chain bounty id.
    pub bounty_id: String,
    /// Attestation id that satisfied the bounty predicate.
    pub attestation_id: String,
    /// Payout amount in nanos, encoded as decimal string to preserve precision.
    pub payout: String,
    /// Attester address (`lig1...`).
    pub attester: String,
}

/// `Bounty/BountyDisputed` event wrapper emitted by the bounty module.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyBountyDisputedEvent {
    /// Inner event payload.
    pub bounty_disputed: BountyDisputedPayload,
}

/// Inner `BountyDisputed` payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyDisputedPayload {
    /// Chain bounty id.
    pub bounty_id: String,
    /// Disputed attestation id.
    pub attestation_id: String,
    /// Disputer address (`lig1...`).
    pub disputer: String,
    /// Bond amount in nanos, encoded as decimal string to preserve precision.
    pub bond: String,
}

/// `Bounty/DisputeResolved` event wrapper emitted by the bounty module.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyDisputeResolvedEvent {
    /// Inner event payload.
    pub dispute_resolved: BountyDisputeResolvedPayload,
}

/// Inner `DisputeResolved` payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyDisputeResolvedPayload {
    /// Chain bounty id.
    pub bounty_id: String,
    /// Resolved attestation id.
    pub attestation_id: String,
    /// Chain decision string.
    pub decision: String,
    /// Address receiving the dispute bond (`lig1...`).
    pub bond_recipient: String,
}

/// `Bounty/BountyExpired` event wrapper emitted by the bounty module.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyBountyExpiredEvent {
    /// Inner event payload.
    pub bounty_expired: BountyExpiredPayload,
}

/// Inner `BountyExpired` payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BountyExpiredPayload {
    /// Chain bounty id.
    pub bounty_id: String,
    /// Poster/refund recipient address (`lig1...`).
    pub refunded_to_poster: String,
}
