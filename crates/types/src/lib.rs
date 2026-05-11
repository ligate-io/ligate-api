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
    /// Wallet/explorer-facing chain id, e.g. `ligate-devnet-1`.
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
    /// Balance in base units (nanos for `$LGT`), serialized as a string
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

/// `(amount, token_id)` pair as emitted by the bank module.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Coins {
    /// Amount as a decimal string (chain uses u128, JS-compat).
    pub amount: String,
    /// Bech32m token id (`token_1...`).
    pub token_id: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_info_round_trip() {
        let body = r#"{"chain_id":"ligate-devnet-1","chain_hash":"abcd","version":"0.0.1"}"#;
        let parsed: RollupInfo = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.chain_id, "ligate-devnet-1");
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
