//! Tx-classifier: lifts chain `LedgerEvent`s into typed [`IndexerTx`]s
//! that match RFC 0002's `Tx.kind` discriminator.
//!
//! ## Why event-driven (and not body-driven)
//!
//! The chain's `LedgerTx.body.data` field is empty in current public
//! releases — the chain elides borsh-encoded body bytes from JSON
//! responses to avoid leaking pre-finalisation internals. So the
//! indexer can't deserialise the runtime call directly.
//!
//! Instead, every runtime call emits one or more events as a
//! side-effect of execution (the chain's accounts / bank / attestation
//! modules each emit typed events). The events carry enough info to
//! reconstruct what happened — for a transfer, `Bank/TokenTransferred`
//! has from / to / amount / token_id, which is exactly what RFC 0002's
//! `Tx { kind: "transfer", details: { to, amount_nano, token_id } }`
//! shape needs.
//!
//! ## Coverage
//!
//! v1 of the parser handles:
//!
//! - `Bank/TokenTransferred` -> [`IndexerTx::Transfer`]
//!
//! Other tx kinds (register-attestor-set, register-schema,
//! submit-attestation) fall through to [`IndexerTx::Unknown`] and are
//! ingested with `kind = "unknown"`. Follow-up PRs add typed parsers
//! per-kind once each is observed against a localnet.
//!
//! ## Mapping receipts -> RFC 0002 `outcome`
//!
//! - chain `result = "successful"` -> RFC `outcome = "committed"`
//! - chain `result = "reverted"`   -> RFC `outcome = "reverted"`
//! - chain `result = "skipped"`    -> indexer DROPS the tx (skipped txs
//!   weren't actually applied; storing them would create misleading
//!   activity history)

use ligate_api_types::{
    AttestationAttestationSubmittedEvent, AttestationAttestorSetRegisteredEvent,
    AttestationSchemaRegisteredEvent, BankTokenTransferredEvent, BountyClaimedEvent,
    BountyDisputedEvent, BountyExpiredEvent, BountyFinalisedEvent, BountyPostedEvent,
    ContractCancelledEvent, ContractDeliveredEvent, ContractDisputeResolvedEvent,
    ContractExpiredEvent, ContractPostedEvent, DeliveryAcceptedEvent, DeliveryRejectedEvent,
    DisputeResolvedEvent, LedgerEvent, LedgerTx, WorkerCommittedEvent,
};

/// Event-key constant for the Bank module's `TokenTransferred` event.
const KEY_BANK_TOKEN_TRANSFERRED: &str = "Bank/TokenTransferred";

/// Event keys emitted by the Bounty module (chain#519, ligate-chain
/// v0.4.0+). The bounty module reports its `Display` name as plain
/// `Bounty` (like the bank module overrides to `Bank`), so the prefix
/// is `Bounty/` rather than `BountyModule/`. The strings match the
/// auto-generated `"<Module>/<VariantName>"` form the SDK's
/// `emit_event` produces.
const KEY_BOUNTY_POSTED: &str = "Bounty/BountyPosted";
const KEY_BOUNTY_CLAIMED: &str = "Bounty/BountyClaimed";
const KEY_BOUNTY_DISPUTED: &str = "Bounty/BountyDisputed";
const KEY_BOUNTY_DISPUTE_RESOLVED: &str = "Bounty/DisputeResolved";
const KEY_BOUNTY_EXPIRED: &str = "Bounty/BountyExpired";
const KEY_BOUNTY_FINALISED: &str = "Bounty/BountyFinalised";

/// Event keys emitted by the Contract module (chain contract primitive,
/// ligate-chain v0.4.0+). **Critical:** the contract module's struct is
/// named `Contracts` (plural — see ligate-chain
/// `crates/modules/contract/src/lib.rs`), and the SDK derives the
/// event-key prefix from the struct identifier verbatim
/// (`format!("{struct_ident}/{variant}")`, per
/// `sov-modules-api::module::event_key` + the `ModuleInfo` derive's
/// `ModulePrefix::new_module(.., stringify!(#struct_ident))`). So the
/// prefix is `Contracts/`, NOT `Contract/`. (Bounty's struct is
/// `Bounty`, hence `Bounty/`.) Getting this wrong = a silent miss where
/// every contract event falls through to `Unknown`. Verified against
/// the SDK macro source + the `CONTRACTS_DISCRIMINANT` (ScreamingSnake
/// of `Contracts`) in `constants.toml`.
const KEY_CONTRACT_POSTED: &str = "Contracts/ContractPosted";
const KEY_CONTRACT_WORKER_COMMITTED: &str = "Contracts/WorkerCommitted";
const KEY_CONTRACT_DELIVERED: &str = "Contracts/ContractDelivered";
const KEY_CONTRACT_DELIVERY_ACCEPTED: &str = "Contracts/DeliveryAccepted";
const KEY_CONTRACT_DELIVERY_REJECTED: &str = "Contracts/DeliveryRejected";
const KEY_CONTRACT_DISPUTE_RESOLVED: &str = "Contracts/ContractDisputeResolved";
const KEY_CONTRACT_CANCELLED: &str = "Contracts/ContractCancelled";
const KEY_CONTRACT_EXPIRED: &str = "Contracts/ContractExpired";

/// Event keys emitted by the Attestation module's three CallMessage
/// paths (ligate-chain PR #297). The strings match the auto-generated
/// `"<Module>/<VariantName>"` form the SDK's `emit_event` produces.
// Chain-emitted event keys use the module's `Display` name as the
// prefix. The attestation module reports as `AttestationModule`
// (the `Module` suffix is `sov-modules-api`'s default; bank
// happens to override its name to plain `Bank`, which is the
// inconsistency we have to absorb here). If the chain unifies these
// later the constants change here; nothing else does.
const KEY_ATTESTATION_ATTESTOR_SET_REGISTERED: &str = "AttestationModule/AttestorSetRegistered";
const KEY_ATTESTATION_SCHEMA_REGISTERED: &str = "AttestationModule/SchemaRegistered";
const KEY_ATTESTATION_ATTESTATION_SUBMITTED: &str = "AttestationModule/AttestationSubmitted";

/// Tx outcome from the chain receipt's `result` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome {
    /// Mapped to RFC 0002 `outcome = "committed"`.
    Committed,
    /// Mapped to RFC 0002 `outcome = "reverted"`.
    Reverted,
    /// Skipped txs weren't applied — indexer DROPS them. Returning
    /// this variant from [`outcome_of`] tells the caller "do not
    /// insert."
    Skipped,
}

/// Lift a chain `result` string into a typed [`TxOutcome`]. Unknown
/// values (a future chain release adds a fourth variant) get
/// `Skipped` so the indexer fails closed: a tx whose outcome we can't
/// classify shouldn't be persisted as if we knew it succeeded.
pub fn outcome_of(receipt_result: &str) -> TxOutcome {
    match receipt_result {
        "successful" => TxOutcome::Committed,
        "reverted" => TxOutcome::Reverted,
        _ => TxOutcome::Skipped,
    }
}

/// Decoded representation of one chain tx, matching RFC 0002's
/// `Tx.kind` + `Tx.details` discriminator.
///
/// Kinds the parser doesn't yet recognise become [`IndexerTx::Unknown`]
/// rather than failing the ingest — this keeps a chain wire-format
/// shift from stalling the indexer mid-slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexerTx {
    /// `Bank/TokenTransferred`.
    Transfer(IndexerTransfer),
    /// `Attestation/AttestorSetRegistered`. Mirrors RFC 0002's
    /// `details` shape for `kind = "register_attestor_set"`.
    RegisterAttestorSet(IndexerRegisterAttestorSet),
    /// `Attestation/SchemaRegistered`. Mirrors RFC 0002's `details`
    /// shape for `kind = "register_schema"`.
    RegisterSchema(IndexerRegisterSchema),
    /// `Attestation/AttestationSubmitted`. Mirrors RFC 0002's
    /// `details` shape for `kind = "submit_attestation"`.
    SubmitAttestation(IndexerSubmitAttestation),
    /// A `Bounty/*` lifecycle event. Carries the affected bounty id
    /// plus a [`BountyEventKind`] discriminator the ingest step uses to
    /// decide which per-event delta to apply on top of the
    /// re-hydrated record. See [`BountyEventKind`] for why the kind is
    /// "first bounty event in the tx" + summed claim accounting.
    BountyEvent {
        /// Bech32m `lbt1...` id of the bounty the tx touched.
        bounty_id: String,
        /// What happened to the bounty in this tx.
        kind: BountyEventKind,
    },
    /// A `Contracts/*` lifecycle event. Carries the affected contract id
    /// plus a [`ContractEventKind`] discriminator the ingest step uses
    /// to decide which per-event delta to apply on top of the
    /// re-hydrated record (escrow-zeroing on terminal states). See
    /// [`ContractEventKind`]; the contract id + kind come from the first
    /// recognised contract event in the tx.
    ContractEvent {
        /// Bech32m `lct1...` id of the contract the tx touched.
        contract_id: String,
        /// What happened to the contract in this tx.
        kind: ContractEventKind,
    },
    /// Catch-all. Either no events were emitted (e.g. a no-op tx), or
    /// the events present don't match any kind the parser knows. The
    /// indexer writes this as `kind = "unknown"` with the raw event
    /// keys captured in `details.event_keys` for forensic lookups.
    Unknown { event_keys: Vec<String> },
}

/// Which bounty lifecycle transition a [`IndexerTx::BountyEvent`]
/// represents.
///
/// **Batch claims.** A single `ClaimBounty` tx can emit multiple
/// `Bounty/BountyClaimed` events (one per attestation claimed in the
/// batch). Rather than emit one `BountyEvent` per claim, the parser
/// collapses them into a single `Claimed` carrying `count` (number of
/// `BountyClaimed` events in the tx) and `total_payout` (their summed
/// payouts), so the ingest step can decrement escrow and bump
/// `claim_count` correctly in one pass. The hydrate re-reads the
/// authoritative `status` regardless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BountyEventKind {
    /// `Bounty/BountyPosted`.
    Posted,
    /// One or more `Bounty/BountyClaimed` in the same tx.
    Claimed {
        /// Number of `BountyClaimed` events in the tx (>= 1).
        count: u32,
        /// Sum of the `payout` fields across those events, as a u128
        /// decimal string. Postgres numeric arithmetic decrements
        /// `escrow_remaining_nano` by this at ingest.
        total_payout: String,
    },
    /// `Bounty/BountyDisputed`.
    Disputed,
    /// `Bounty/DisputeResolved`.
    DisputeResolved,
    /// `Bounty/BountyExpired`.
    Expired,
    /// `Bounty/BountyFinalised`.
    Finalised,
}

/// Which contract lifecycle transition a [`IndexerTx::ContractEvent`]
/// represents.
///
/// Unlike bounty claims, contract events are NOT batched (one contract
/// per tx, one lifecycle transition per tx), so this is a plain
/// discriminator with no summed accounting. The ingest step re-hydrates
/// the authoritative `status` from chain state on every event; the kind
/// only decides whether to additionally zero the escrow column (the
/// terminal `Accepted` / `Cancelled` / `Expired` / dispute-`Resolved`
/// states drain escrow on-chain via payout / refund).
///
/// **`Accepted` covers two chain calls.** Both the poster's explicit
/// `AcceptDelivery` and the permissionless `FinalizeDelivery` auto-accept
/// sweep emit the same `Contracts/DeliveryAccepted` event, so they map to
/// the same `Accepted` kind here — the indexer can't (and needn't)
/// distinguish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractEventKind {
    /// `Contracts/ContractPosted`.
    Posted,
    /// `Contracts/WorkerCommitted`.
    Committed,
    /// `Contracts/ContractDelivered`.
    Delivered,
    /// `Contracts/DeliveryAccepted` (poster-accept OR auto-accept
    /// finalize). Terminal: escrow drained to the worker on-chain.
    Accepted,
    /// `Contracts/DeliveryRejected`. Transitions to Disputed; escrow
    /// stays locked until the arbiter resolves.
    Rejected,
    /// `Contracts/ContractDisputeResolved`. Terminal: pool + bond
    /// settled per the arbiter's decision; escrow drained on-chain.
    DisputeResolved,
    /// `Contracts/ContractCancelled`. Terminal: escrow refunded to the
    /// poster on-chain.
    Cancelled,
    /// `Contracts/ContractExpired`. Terminal: escrow refunded to the
    /// poster on-chain.
    Expired,
}

/// Decoded transfer details. Mirrors RFC 0002's `Tx.details` shape for
/// `kind = "transfer"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerTransfer {
    /// Sender, bech32m `lig1...`.
    pub from: String,
    /// Recipient, bech32m `lig1...`.
    pub to: String,
    /// Amount in nanos as a decimal string (preserves u128 precision).
    pub amount_nano: String,
    /// Bech32m token id (`token_1...`).
    pub token_id: String,
}

/// Decoded `register_attestor_set` details. Drives both the
/// `transactions.details` JSONB and the `attestor_sets` row insert
/// downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerRegisterAttestorSet {
    /// Bech32m `las1...` deterministic id.
    pub attestor_set_id: String,
    /// Member pubkeys (bech32m `lpk1...`), post-canonicalisation order.
    pub members: Vec<String>,
    /// M-of-N threshold.
    pub threshold: u8,
    /// Tx sender (paid the registration fee). Bech32m `lig1...`.
    pub registered_by: String,
}

/// Decoded `register_schema` details. Carries every column the
/// `schemas` table requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerRegisterSchema {
    /// Bech32m `lsc1...` deterministic id.
    pub schema_id: String,
    /// Schema name (e.g. `themisra.proof-of-prompt`).
    pub name: String,
    /// Schema version (monotonic per name+owner).
    pub version: u32,
    /// Owner address (bech32m `lig1...`).
    pub owner: String,
    /// Bound attestor set id (bech32m `las1...`).
    pub attestor_set_id: String,
    /// Fee-routing share in basis points.
    pub fee_routing_bps: u16,
    /// Destination address for the routed share. `None` iff bps == 0.
    pub fee_routing_addr: Option<String>,
    /// SHA-256 of canonical schema-doc bytes. Stringified from
    /// whichever serialisation the chain emitted (typically hex with
    /// or without `0x`; bech32m wrap later possible).
    pub payload_shape_hash: String,
}

/// Decoded `submit_attestation` details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerSubmitAttestation {
    /// Canonical `lat1...` AttestationId derived from
    /// `(schema_id, payload_hash)` via
    /// [`crate::attestation_id::compute_attestation_id`]. Matches the
    /// chain's `AttestationId::from_pair(...).to_string()`.
    pub id: String,
    /// Schema id (bech32m `lsc1...`).
    pub schema_id: String,
    /// Payload hash (bech32m `lph1...`).
    pub payload_hash: String,
    /// Submitter address (bech32m `lig1...`).
    pub submitter: String,
    /// Number of signatures included with the submission.
    pub signature_count: u32,
}

/// Classify a `LedgerTx` plus its emitted events into an [`IndexerTx`].
///
/// Returns `None` if the tx was [`TxOutcome::Skipped`] — the caller
/// should not persist anything in that case.
///
/// `events` should be every `LedgerEvent` whose `tx_hash` matches
/// `tx.hash`. Caller is responsible for the filter (typically: fetch
/// `/v1/ledger/slots/{n}/events` once, group by `tx_hash`).
/// Normalise a tx-hash string to a canonical 64-char lowercase-hex
/// form (no `0x` prefix), regardless of whether the input is
pub fn classify_tx(tx: &LedgerTx, events: &[&LedgerEvent]) -> Option<ClassifiedTx> {
    let outcome = outcome_of(&tx.receipt.result);
    if outcome == TxOutcome::Skipped {
        return None;
    }

    let kind = classify_events(events);
    Some(ClassifiedTx {
        hash: tx.hash.clone(),
        batch_number: tx.batch_number,
        global_tx_number: tx.number,
        outcome,
        kind,
    })
}

/// One classified tx, ready for [`crate::db`] insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTx {
    /// Tx hash as the chain serialised it. Bech32m `ltx1...` on
    /// `ligate-chain` `0ac7e5b` and later; hex `0x...` on older chain
    /// revs. The parser doesn't validate the format.
    pub hash: String,
    /// `LedgerTx.batch_number`. Resolves to slot via the
    /// `/v1/ledger/batches/{n}` lookup.
    pub batch_number: u64,
    /// `LedgerTx.number` — global tx index. Position-in-batch is
    /// derivable as `global_tx_number - batch.tx_range.start`.
    pub global_tx_number: u64,
    /// Mapped from `receipt.result`.
    pub outcome: TxOutcome,
    /// Decoded body (or [`IndexerTx::Unknown`] if no parser matched).
    pub kind: IndexerTx,
}

/// Classify a tx's emitted events into a typed [`IndexerTx`].
///
/// Order of preference (first match wins):
///
/// 1. `Attestation/AttestorSetRegistered` -> [`IndexerTx::RegisterAttestorSet`]
/// 2. `Attestation/SchemaRegistered`      -> [`IndexerTx::RegisterSchema`]
/// 3. `Attestation/AttestationSubmitted`  -> [`IndexerTx::SubmitAttestation`]
/// 4. any `Bounty/*` event                -> [`IndexerTx::BountyEvent`]
/// 5. any `Contracts/*` event             -> [`IndexerTx::ContractEvent`]
/// 6. `Bank/TokenTransferred`             -> [`IndexerTx::Transfer`]
/// 7. otherwise -> [`IndexerTx::Unknown`] capturing the event keys
///    we saw, for forensic lookup
///
/// **Semantic events win over Bank events.** A `register_schema` tx
/// emits both an `Attestation/SchemaRegistered` (semantic) and a
/// `Bank/TokenTransferred` (the fee payment to treasury); a
/// `PostBounty` tx emits a `Bounty/BountyPosted` AND a
/// `Bank/TokenTransferred` for the escrow deposit; a `PostContract` tx
/// emits a `Contracts/ContractPosted` AND a `Bank/TokenTransferred` for
/// the escrow deposit. Without the preference the parser would classify
/// any of them as a Transfer of the fee/escrow, dropping the semantic
/// event. We walk events looking for Attestation- then Bounty- then
/// Contract-module keys first; only if none is found do we fall through
/// to the bank check.
fn classify_events(events: &[&LedgerEvent]) -> IndexerTx {
    // Pass 1: look for an Attestation-module event. These are the
    // semantic events for the register/submit call types.
    for ev in events {
        match ev.key.as_str() {
            KEY_ATTESTATION_ATTESTOR_SET_REGISTERED => {
                if let Ok(payload) = serde_json::from_value::<AttestationAttestorSetRegisteredEvent>(
                    ev.value.clone(),
                ) {
                    let d = payload.attestor_set_registered;
                    return IndexerTx::RegisterAttestorSet(IndexerRegisterAttestorSet {
                        attestor_set_id: d.attestor_set_id,
                        members: d.members,
                        threshold: d.threshold,
                        registered_by: d.registered_by,
                    });
                }
            }
            KEY_ATTESTATION_SCHEMA_REGISTERED => {
                if let Ok(payload) =
                    serde_json::from_value::<AttestationSchemaRegisteredEvent>(ev.value.clone())
                {
                    let d = payload.schema_registered;
                    return IndexerTx::RegisterSchema(IndexerRegisterSchema {
                        schema_id: d.schema_id,
                        name: d.name,
                        version: d.version,
                        owner: d.owner,
                        attestor_set_id: d.attestor_set_id,
                        fee_routing_bps: d.fee_routing_bps,
                        fee_routing_addr: d.fee_routing_addr,
                        // `payload_shape_hash` is `Value` in the typed
                        // event payload (chain serialisation form
                        // varies across revs); stringify-then-strip
                        // surrounding quotes if it's already a string,
                        // or fall back to the JSON repr.
                        payload_shape_hash: match d.payload_shape_hash {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        },
                    });
                }
            }
            KEY_ATTESTATION_ATTESTATION_SUBMITTED => {
                if let Ok(payload) =
                    serde_json::from_value::<AttestationAttestationSubmittedEvent>(ev.value.clone())
                {
                    let d = payload.attestation_submitted;
                    // Collapse `(schema_id, payload_hash)` into the
                    // canonical v0.2.0 `lat1...` AttestationId at
                    // ingest time. Chain emits the pair; the indexer
                    // mirrors the chain's `AttestationId::from_pair`
                    // derivation so reads can resolve by id directly.
                    match crate::attestation_id::compute_attestation_id(
                        &d.schema_id,
                        &d.payload_hash,
                    ) {
                        Ok(id) => {
                            return IndexerTx::SubmitAttestation(IndexerSubmitAttestation {
                                id,
                                schema_id: d.schema_id,
                                payload_hash: d.payload_hash,
                                submitter: d.submitter,
                                signature_count: d.signature_count,
                            });
                        }
                        Err(e) => {
                            // Chain emitted a malformed pair (would
                            // be a chain-side regression). Skip the
                            // event rather than crash; the tx falls
                            // through to `Unknown` and shows up in
                            // forensics via `event_keys`.
                            tracing::warn!(
                                error = %e,
                                schema_id = %d.schema_id,
                                payload_hash = %d.payload_hash,
                                "AttestationSubmitted: cannot derive lat1 id, skipping",
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Pass 2: look for Bounty-module events. These are semantic events
    // for the bounty CallMessage paths and win over the
    // `Bank/TokenTransferred` a PostBounty / refund tx also emits.
    //
    // A batch `ClaimBounty` can emit multiple `BountyClaimed` events in
    // one tx. We collect them: take the first bounty event's id + kind
    // as the representative classification, but for claims also sum the
    // payouts and count the events so the ingest step can apply the
    // batch's escrow decrement + claim_count bump in one shot.
    if let Some(bounty) = classify_bounty_events(events) {
        return bounty;
    }

    // Pass 3: look for Contract-module events. Same rationale as the
    // bounty pass — a PostContract / payout / refund tx also emits a
    // `Bank/TokenTransferred`, and the semantic contract event must win.
    if let Some(contract) = classify_contract_events(events) {
        return contract;
    }

    // Pass 4: no semantic event matched; fall back to the bank check
    // for plain transfers.
    for ev in events {
        if ev.key == KEY_BANK_TOKEN_TRANSFERRED {
            // The serde_json::from_value path picks up the typed shape
            // from `ligate-api-types`. If decoding fails, we treat the
            // event as opaque rather than panicking — the indexer must
            // never crash mid-slot.
            if let Ok(payload) =
                serde_json::from_value::<BankTokenTransferredEvent>(ev.value.clone())
            {
                return IndexerTx::Transfer(IndexerTransfer {
                    from: payload.token_transferred.from.user,
                    to: payload.token_transferred.to.user,
                    amount_nano: payload.token_transferred.coins.amount,
                    token_id: payload.token_transferred.coins.token_id,
                });
            }
            // Fall through to Unknown below if decode failed; we still
            // record the event key for forensics.
        }
    }
    IndexerTx::Unknown {
        event_keys: events.iter().map(|e| e.key.clone()).collect(),
    }
}

/// Scan `events` for `Bounty/*` keys and, if any are present, collapse
/// them into a single [`IndexerTx::BountyEvent`].
///
/// Returns `None` when no bounty event is present (caller falls
/// through to the bank check). The bounty id and discriminator come
/// from the FIRST recognised bounty event in the tx; the ingest step
/// re-hydrates the full record by id, so a single representative
/// classification is enough for everything except claim accounting.
///
/// **Claim accounting.** A batch `ClaimBounty` emits one
/// `BountyClaimed` per attestation. We sum every `BountyClaimed`
/// payout in the tx into `total_payout` and count them into `count`,
/// regardless of which bounty event happened to be first — in
/// practice a tx touches a single bounty, so all `BountyClaimed`
/// events share the same `bounty_id`. If a future chain rev batches
/// claims across multiple bounties in one tx, this would under-count;
/// that's flagged as a v1 limitation (the per-bounty hydrate still
/// fixes `status`, only the escrow delta would be approximate).
fn classify_bounty_events(events: &[&LedgerEvent]) -> Option<IndexerTx> {
    // First recognised bounty event drives id + kind. We decode lazily
    // and tolerate a malformed payload by skipping that event (the tx
    // falls through to Unknown if NO bounty event decodes), matching
    // the never-crash-mid-slot policy of the attestation path.
    let mut first: Option<(String, BountyEventKind)> = None;
    // Claim tally, summed across every BountyClaimed in the tx.
    let mut claim_count: u32 = 0;
    let mut claim_total = 0u128;
    let mut saw_claim = false;

    for ev in events {
        match ev.key.as_str() {
            KEY_BOUNTY_POSTED => {
                if let Ok(p) = serde_json::from_value::<BountyPostedEvent>(ev.value.clone()) {
                    first.get_or_insert((p.bounty_posted.bounty_id, BountyEventKind::Posted));
                }
            }
            KEY_BOUNTY_CLAIMED => {
                if let Ok(p) = serde_json::from_value::<BountyClaimedEvent>(ev.value.clone()) {
                    let d = p.bounty_claimed;
                    claim_count += 1;
                    saw_claim = true;
                    // Sum payouts in u128; a malformed/overflowing
                    // amount is skipped from the sum but still counted
                    // (the hydrate-driven status refresh is unaffected).
                    if let Ok(v) = d.payout.parse::<u128>() {
                        claim_total = claim_total.saturating_add(v);
                    }
                    // Placeholder kind; replaced by the summed Claimed
                    // below once we've walked every event.
                    first.get_or_insert((d.bounty_id, BountyEventKind::Posted));
                }
            }
            KEY_BOUNTY_DISPUTED => {
                if let Ok(p) = serde_json::from_value::<BountyDisputedEvent>(ev.value.clone()) {
                    first.get_or_insert((p.bounty_disputed.bounty_id, BountyEventKind::Disputed));
                }
            }
            KEY_BOUNTY_DISPUTE_RESOLVED => {
                if let Ok(p) = serde_json::from_value::<DisputeResolvedEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.dispute_resolved.bounty_id,
                        BountyEventKind::DisputeResolved,
                    ));
                }
            }
            KEY_BOUNTY_EXPIRED => {
                if let Ok(p) = serde_json::from_value::<BountyExpiredEvent>(ev.value.clone()) {
                    first.get_or_insert((p.bounty_expired.bounty_id, BountyEventKind::Expired));
                }
            }
            KEY_BOUNTY_FINALISED => {
                if let Ok(p) = serde_json::from_value::<BountyFinalisedEvent>(ev.value.clone()) {
                    first.get_or_insert((p.bounty_finalised.bounty_id, BountyEventKind::Finalised));
                }
            }
            _ => {}
        }
    }

    let (bounty_id, kind) = first?;
    // If any BountyClaimed was seen, the tx is a claim regardless of
    // which event sorted first — promote to the summed Claimed kind.
    let kind = if saw_claim {
        BountyEventKind::Claimed {
            count: claim_count,
            total_payout: claim_total.to_string(),
        }
    } else {
        kind
    };
    Some(IndexerTx::BountyEvent { bounty_id, kind })
}

/// Scan `events` for `Contracts/*` keys and, if any are present,
/// collapse them into a single [`IndexerTx::ContractEvent`].
///
/// Returns `None` when no contract event is present (caller falls
/// through to the bank check). The contract id and discriminator come
/// from the FIRST recognised contract event in the tx; the ingest step
/// re-hydrates the full record by id, so a single representative
/// classification is enough. Unlike bounty claims, contract lifecycle
/// txs aren't batched (one contract, one transition per tx), so there's
/// no summed accounting here.
///
/// Decodes lazily and tolerates a malformed payload by skipping that
/// event (the tx falls through to `Unknown` if NO contract event
/// decodes), matching the never-crash-mid-slot policy of the
/// attestation + bounty paths.
fn classify_contract_events(events: &[&LedgerEvent]) -> Option<IndexerTx> {
    let mut first: Option<(String, ContractEventKind)> = None;

    for ev in events {
        match ev.key.as_str() {
            KEY_CONTRACT_POSTED => {
                if let Ok(p) = serde_json::from_value::<ContractPostedEvent>(ev.value.clone()) {
                    first.get_or_insert((p.contract_posted.contract_id, ContractEventKind::Posted));
                }
            }
            KEY_CONTRACT_WORKER_COMMITTED => {
                if let Ok(p) = serde_json::from_value::<WorkerCommittedEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.worker_committed.contract_id,
                        ContractEventKind::Committed,
                    ));
                }
            }
            KEY_CONTRACT_DELIVERED => {
                if let Ok(p) = serde_json::from_value::<ContractDeliveredEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.contract_delivered.contract_id,
                        ContractEventKind::Delivered,
                    ));
                }
            }
            KEY_CONTRACT_DELIVERY_ACCEPTED => {
                if let Ok(p) = serde_json::from_value::<DeliveryAcceptedEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.delivery_accepted.contract_id,
                        ContractEventKind::Accepted,
                    ));
                }
            }
            KEY_CONTRACT_DELIVERY_REJECTED => {
                if let Ok(p) = serde_json::from_value::<DeliveryRejectedEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.delivery_rejected.contract_id,
                        ContractEventKind::Rejected,
                    ));
                }
            }
            KEY_CONTRACT_DISPUTE_RESOLVED => {
                if let Ok(p) =
                    serde_json::from_value::<ContractDisputeResolvedEvent>(ev.value.clone())
                {
                    first.get_or_insert((
                        p.contract_dispute_resolved.contract_id,
                        ContractEventKind::DisputeResolved,
                    ));
                }
            }
            KEY_CONTRACT_CANCELLED => {
                if let Ok(p) = serde_json::from_value::<ContractCancelledEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.contract_cancelled.contract_id,
                        ContractEventKind::Cancelled,
                    ));
                }
            }
            KEY_CONTRACT_EXPIRED => {
                if let Ok(p) = serde_json::from_value::<ContractExpiredEvent>(ev.value.clone()) {
                    first.get_or_insert((
                        p.contract_expired.contract_id,
                        ContractEventKind::Expired,
                    ));
                }
            }
            _ => {}
        }
    }

    let (contract_id, kind) = first?;
    Some(IndexerTx::ContractEvent { contract_id, kind })
}

#[cfg(test)]
mod tests {
    use bech32::{Bech32m, Hrp};
    use ligate_api_types::{FullyBakedTx, TxReceipt, Uint64Range};

    use super::*;
    use crate::attestation_id::compute_attestation_id;

    /// bech32m-encode `data` under `hrp`. Used to mint valid
    /// `lsc1.../lph1...` fixtures the new parser path can decode.
    fn bech32m(hrp: &str, data: &[u8]) -> String {
        let hrp = Hrp::parse(hrp).unwrap();
        bech32::encode::<Bech32m>(hrp, data).unwrap()
    }

    fn fixture_tx(receipt_result: &str) -> LedgerTx {
        LedgerTx {
            r#type: "tx".into(),
            hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(), // synthetic bech32m fixture, format-opaque to parser
            number: 1,
            event_range: Uint64Range { start: 1, end: 2 },
            body: FullyBakedTx {
                data: String::new(),
                sequencing_data: None,
            },
            receipt: TxReceipt {
                result: receipt_result.into(),
                data: serde_json::json!({"gas_used": [0, 0]}),
            },
            events: vec![],
            batch_number: 8929,
        }
    }

    #[test]
    fn outcome_maps_chain_strings_to_typed_variant() {
        assert_eq!(outcome_of("successful"), TxOutcome::Committed);
        assert_eq!(outcome_of("reverted"), TxOutcome::Reverted);
        assert_eq!(outcome_of("skipped"), TxOutcome::Skipped);
        // Forward-compat: unknown -> Skipped (fail closed).
        assert_eq!(outcome_of("future-variant"), TxOutcome::Skipped);
    }

    #[test]
    fn classify_drops_skipped_txs() {
        let tx = fixture_tx("skipped");
        let events: Vec<&LedgerEvent> = vec![];
        assert!(classify_tx(&tx, &events).is_none());
    }

    #[test]
    fn classify_recognises_token_transferred() {
        // Wire shape captured from a localnet tx — kept as-is so the
        // test pins what we observed against ligate-localnet (chain
        // ligate-localnet, slot 8975, tx_hash `ltx1...` (chain-localnet, slot 8975)).
        let event_value = serde_json::json!({
            "token_transferred": {
                "from": { "user": "lig132yw8ht5p8cetl2jmvknewjawt9xwzdlrk2pyxlnwjyqz3m499u" },
                "to":   { "user": "lig13x2xvtj2n3g5zdrc2g27uswja0e9dllxlu33y8estm0gw4dhs6d" },
                "coins": {
                    "amount":   "1000000000",
                    "token_id": "token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7"
                }
            }
        });
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: KEY_BANK_TOKEN_TRANSFERRED.into(),
            value: event_value,
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Bank".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(), // synthetic bech32m fixture, format-opaque to parser
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        assert_eq!(classified.outcome, TxOutcome::Committed);
        match classified.kind {
            IndexerTx::Transfer(t) => {
                assert_eq!(
                    t.from,
                    "lig132yw8ht5p8cetl2jmvknewjawt9xwzdlrk2pyxlnwjyqz3m499u"
                );
                assert_eq!(
                    t.to,
                    "lig13x2xvtj2n3g5zdrc2g27uswja0e9dllxlu33y8estm0gw4dhs6d"
                );
                assert_eq!(t.amount_nano, "1000000000");
                assert_eq!(
                    t.token_id,
                    "token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7"
                );
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }

    #[test]
    fn classify_falls_back_to_unknown_for_unrecognised_events() {
        // Use a future / typo'd module key so the parser has no
        // typed handler — confirms the catch-all path still surfaces
        // event keys for forensics.
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "Future/SomethingNew".into(),
            value: serde_json::json!({"some_payload": {}}),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Future".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::Unknown { event_keys } => {
                assert_eq!(event_keys, vec!["Future/SomethingNew"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_attestor_set_registered() {
        // Mirrors the shape `AttestationModule/AttestorSetRegistered`
        // serialises to on the chain's REST surface: externally-tagged
        // enum with the PascalCase variant name as the JSON key, and
        // raw bech32m strings for address fields (NOT the bank module's
        // `{"user": "lig1..."}` wrapper). The constants in this file
        // + the serde renames in `ligate-api-types` encode that shape.
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "AttestationModule/AttestorSetRegistered".into(),
            value: serde_json::json!({
                "AttestorSetRegistered": {
                    "attestor_set_id": "las1abc",
                    "members": ["lpk1m1", "lpk1m2"],
                    "threshold": 2,
                    "registered_by": "lig1registrar"
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "AttestationModule".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::RegisterAttestorSet(d) => {
                assert_eq!(d.attestor_set_id, "las1abc");
                assert_eq!(d.members, vec!["lpk1m1", "lpk1m2"]);
                assert_eq!(d.threshold, 2);
                assert_eq!(d.registered_by, "lig1registrar");
            }
            other => panic!("expected RegisterAttestorSet, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_schema_registered() {
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "AttestationModule/SchemaRegistered".into(),
            value: serde_json::json!({
                "SchemaRegistered": {
                    "schema_id": "lsc1abc",
                    "name": "themisra.proof-of-prompt",
                    "version": 1,
                    "owner": "lig1owner",
                    "attestor_set_id": "las1abc",
                    "fee_routing_bps": 0,
                    "fee_routing_addr": null,
                    "payload_shape_hash": "0xdeadbeef"
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "AttestationModule".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::RegisterSchema(d) => {
                assert_eq!(d.schema_id, "lsc1abc");
                assert_eq!(d.name, "themisra.proof-of-prompt");
                assert_eq!(d.version, 1);
                assert_eq!(d.owner, "lig1owner");
                assert_eq!(d.attestor_set_id, "las1abc");
                assert_eq!(d.fee_routing_bps, 0);
                assert!(d.fee_routing_addr.is_none());
                assert_eq!(d.payload_shape_hash, "0xdeadbeef");
            }
            other => panic!("expected RegisterSchema, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_attestation_submitted() {
        // schema_id + payload_hash must be valid bech32m so the parser
        // can derive the canonical `lat1...` AttestationId at ingest.
        let schema_id = bech32m("lsc", &[0x11u8; 32]);
        let payload_hash = bech32m("lph", &[0x22u8; 32]);
        let expected_id =
            compute_attestation_id(&schema_id, &payload_hash).expect("derive lat1 id");

        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "AttestationModule/AttestationSubmitted".into(),
            value: serde_json::json!({
                "AttestationSubmitted": {
                    "schema_id":       schema_id,
                    "payload_hash":    payload_hash,
                    "submitter":       "lig1submitter",
                    "signature_count": 3
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "AttestationModule".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::SubmitAttestation(d) => {
                assert_eq!(d.id, expected_id, "lat1 id derived at ingest");
                assert!(d.id.starts_with("lat1"));
                assert_eq!(d.schema_id, bech32m("lsc", &[0x11u8; 32]));
                assert_eq!(d.payload_hash, bech32m("lph", &[0x22u8; 32]));
                assert_eq!(d.submitter, "lig1submitter");
                assert_eq!(d.signature_count, 3);
            }
            other => panic!("expected SubmitAttestation, got {other:?}"),
        }
    }

    /// If the chain ever emits a malformed `(schema_id, payload_hash)`
    /// pair (chain-side regression), the parser logs and falls
    /// through to `Unknown` instead of crashing the ingest loop.
    #[test]
    fn submit_attestation_with_malformed_pair_falls_through_to_unknown() {
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "AttestationModule/AttestationSubmitted".into(),
            value: serde_json::json!({
                "AttestationSubmitted": {
                    "schema_id":       "lsc1notbech32m",
                    "payload_hash":    "lph1alsobad",
                    "submitter":       "lig1submitter",
                    "signature_count": 1
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "AttestationModule".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::Unknown { event_keys } => {
                assert_eq!(event_keys, vec!["AttestationModule/AttestationSubmitted"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn attestation_event_wins_over_bank_fee_transfer() {
        // A register_schema tx emits BOTH the SchemaRegistered event
        // AND a Bank/TokenTransferred for the fee payment. The parser
        // must pick the semantic event, not the fee transfer.
        let semantic = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "AttestationModule/SchemaRegistered".into(),
            value: serde_json::json!({
                "SchemaRegistered": {
                    "schema_id": "lsc1abc",
                    "name": "x",
                    "version": 1,
                    "owner": "lig1owner",
                    "attestor_set_id": "las1abc",
                    "fee_routing_bps": 0,
                    "fee_routing_addr": null,
                    "payload_shape_hash": "0x00"
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "AttestationModule".into(),
            },
            tx_hash: "ltx1abc".into(),
        };
        let fee = LedgerEvent {
            r#type: "event".into(),
            number: 2,
            key: KEY_BANK_TOKEN_TRANSFERRED.into(),
            value: serde_json::json!({
                "token_transferred": {
                    "from": {"user": "lig1owner"},
                    "to":   {"user": "lig1treasury"},
                    "coins": {"amount": "100", "token_id": "token_1lgt"}
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Bank".into(),
            },
            tx_hash: "ltx1abc".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&semantic, &fee]).expect("not skipped");
        assert!(matches!(classified.kind, IndexerTx::RegisterSchema(_)));
    }

    // ---- bounty events -----------------------------------------------------

    /// Build a `Bounty/<variant>` ledger event with the given key +
    /// externally-tagged value. Mirrors the inline event construction
    /// the attestation tests use; factored out because the bounty
    /// suite builds several.
    fn bounty_event(number: u64, key: &str, value: serde_json::Value) -> LedgerEvent {
        LedgerEvent {
            r#type: "event".into(),
            number,
            key: key.into(),
            value,
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Bounty".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        }
    }

    #[test]
    fn classify_recognises_bounty_posted() {
        // Externally-tagged enum: PascalCase variant key, raw bech32m
        // addresses (NOT the bank `{"user": ...}` wrapper), u128 string
        // amounts. The constants + serde renames encode that shape.
        let event = bounty_event(
            1,
            KEY_BOUNTY_POSTED,
            serde_json::json!({
                "BountyPosted": {
                    "bounty_id": "lbt1abc",
                    "poster": "lig1poster",
                    "pool": "5000000000"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(kind, BountyEventKind::Posted);
            }
            other => panic!("expected BountyEvent::Posted, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_bounty_claimed() {
        let event = bounty_event(
            1,
            KEY_BOUNTY_CLAIMED,
            serde_json::json!({
                "BountyClaimed": {
                    "bounty_id": "lbt1abc",
                    "attestation_id": "lat1xyz",
                    "payout": "1000000000",
                    "attester": "lig1attester"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(
                    kind,
                    BountyEventKind::Claimed {
                        count: 1,
                        total_payout: "1000000000".into(),
                    }
                );
            }
            other => panic!("expected BountyEvent::Claimed, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_bounty_disputed() {
        let event = bounty_event(
            1,
            KEY_BOUNTY_DISPUTED,
            serde_json::json!({
                "BountyDisputed": {
                    "bounty_id": "lbt1abc",
                    "attestation_id": "lat1xyz",
                    "disputer": "lig1disputer",
                    "bond": "250000000"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(kind, BountyEventKind::Disputed);
            }
            other => panic!("expected BountyEvent::Disputed, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_dispute_resolved() {
        let event = bounty_event(
            1,
            KEY_BOUNTY_DISPUTE_RESOLVED,
            serde_json::json!({
                "DisputeResolved": {
                    "bounty_id": "lbt1abc",
                    "attestation_id": "lat1xyz",
                    "decision": "Accept",
                    "bond_recipient": "lig1disputer"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(kind, BountyEventKind::DisputeResolved);
            }
            other => panic!("expected BountyEvent::DisputeResolved, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_bounty_expired() {
        let event = bounty_event(
            1,
            KEY_BOUNTY_EXPIRED,
            serde_json::json!({
                "BountyExpired": { "bounty_id": "lbt1abc", "refunded_to_poster": "4000000000" }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(kind, BountyEventKind::Expired);
            }
            other => panic!("expected BountyEvent::Expired, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_bounty_finalised() {
        let event = bounty_event(
            1,
            KEY_BOUNTY_FINALISED,
            serde_json::json!({
                "BountyFinalised": { "bounty_id": "lbt1abc", "swept_to_poster": "0" }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(kind, BountyEventKind::Finalised);
            }
            other => panic!("expected BountyEvent::Finalised, got {other:?}"),
        }
    }

    #[test]
    fn classify_batch_claim_sums_payouts_and_counts() {
        // A batch ClaimBounty emits one BountyClaimed per attestation.
        // The parser collapses them into a single Claimed carrying the
        // event count + summed payouts, so the ingest step can apply
        // the escrow decrement and claim_count bump in one pass.
        let claim1 = bounty_event(
            1,
            KEY_BOUNTY_CLAIMED,
            serde_json::json!({
                "BountyClaimed": {
                    "bounty_id": "lbt1abc",
                    "attestation_id": "lat1one",
                    "payout": "1000000000",
                    "attester": "lig1a"
                }
            }),
        );
        let claim2 = bounty_event(
            2,
            KEY_BOUNTY_CLAIMED,
            serde_json::json!({
                "BountyClaimed": {
                    "bounty_id": "lbt1abc",
                    "attestation_id": "lat1two",
                    "payout": "2500000000",
                    "attester": "lig1b"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&claim1, &claim2]).expect("not skipped");
        match classified.kind {
            IndexerTx::BountyEvent { bounty_id, kind } => {
                assert_eq!(bounty_id, "lbt1abc");
                assert_eq!(
                    kind,
                    BountyEventKind::Claimed {
                        count: 2,
                        total_payout: "3500000000".into(),
                    }
                );
            }
            other => panic!("expected batched BountyEvent::Claimed, got {other:?}"),
        }
    }

    #[test]
    fn bounty_event_wins_over_bank_escrow_transfer() {
        // A PostBounty tx emits BOTH a Bounty/BountyPosted (semantic)
        // AND a Bank/TokenTransferred for the escrow deposit. The
        // parser must pick the bounty event, not the escrow transfer.
        let semantic = bounty_event(
            1,
            KEY_BOUNTY_POSTED,
            serde_json::json!({
                "BountyPosted": {
                    "bounty_id": "lbt1abc",
                    "poster": "lig1poster",
                    "pool": "5000000000"
                }
            }),
        );
        let escrow = LedgerEvent {
            r#type: "event".into(),
            number: 2,
            key: KEY_BANK_TOKEN_TRANSFERRED.into(),
            value: serde_json::json!({
                "token_transferred": {
                    "from": {"user": "lig1poster"},
                    "to":   {"user": "lig1escrow"},
                    "coins": {"amount": "5000000000", "token_id": "token_1lgt"}
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Bank".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&semantic, &escrow]).expect("not skipped");
        assert!(matches!(
            classified.kind,
            IndexerTx::BountyEvent {
                kind: BountyEventKind::Posted,
                ..
            }
        ));
    }

    // ---- contract events ---------------------------------------------------

    /// Build a `Contracts/<variant>` ledger event with the given key +
    /// externally-tagged value. NOTE the module name is `Contracts`
    /// (plural) — this is the whole point of the prefix gotcha.
    fn contract_event(number: u64, key: &str, value: serde_json::Value) -> LedgerEvent {
        LedgerEvent {
            r#type: "event".into(),
            number,
            key: key.into(),
            value,
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Contracts".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        }
    }

    #[test]
    fn classify_recognises_contract_posted() {
        // Externally-tagged enum: PascalCase variant key, raw bech32m
        // addresses, u128 string amounts. The `Contracts/` (plural)
        // prefix is what the SDK emits for a module struct named
        // `Contracts`.
        let event = contract_event(
            1,
            KEY_CONTRACT_POSTED,
            serde_json::json!({
                "ContractPosted": {
                    "contract_id": "lct1abc",
                    "poster": "lig1poster",
                    "arbiter": "lig1arbiter",
                    "pool": "5000000000"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Posted);
            }
            other => panic!("expected ContractEvent::Posted, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_worker_committed() {
        let event = contract_event(
            1,
            KEY_CONTRACT_WORKER_COMMITTED,
            serde_json::json!({
                "WorkerCommitted": {
                    "contract_id": "lct1abc",
                    "worker": "lig1worker",
                    "commit_hash": "0xc0ffee",
                    "bond": "250000000"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Committed);
            }
            other => panic!("expected ContractEvent::Committed, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_contract_delivered() {
        let event = contract_event(
            1,
            KEY_CONTRACT_DELIVERED,
            serde_json::json!({
                "ContractDelivered": {
                    "contract_id": "lct1abc",
                    "worker": "lig1worker",
                    "deliverable_attestation_id": "lat1xyz"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Delivered);
            }
            other => panic!("expected ContractEvent::Delivered, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_delivery_accepted() {
        let event = contract_event(
            1,
            KEY_CONTRACT_DELIVERY_ACCEPTED,
            serde_json::json!({
                "DeliveryAccepted": {
                    "contract_id": "lct1abc",
                    "worker": "lig1worker",
                    "payout": "5000000000"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Accepted);
            }
            other => panic!("expected ContractEvent::Accepted, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_delivery_rejected() {
        let event = contract_event(
            1,
            KEY_CONTRACT_DELIVERY_REJECTED,
            serde_json::json!({
                "DeliveryRejected": {
                    "contract_id": "lct1abc",
                    "worker": "lig1worker",
                    "reason": "CriteriaMismatch"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Rejected);
            }
            other => panic!("expected ContractEvent::Rejected, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_contract_dispute_resolved() {
        let event = contract_event(
            1,
            KEY_CONTRACT_DISPUTE_RESOLVED,
            serde_json::json!({
                "ContractDisputeResolved": {
                    "contract_id": "lct1abc",
                    "decision": "AcceptDelivery",
                    "winner": "lig1worker"
                }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::DisputeResolved);
            }
            other => panic!("expected ContractEvent::DisputeResolved, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_contract_cancelled() {
        let event = contract_event(
            1,
            KEY_CONTRACT_CANCELLED,
            serde_json::json!({
                "ContractCancelled": { "contract_id": "lct1abc", "refunded_to_poster": "5000000000" }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Cancelled);
            }
            other => panic!("expected ContractEvent::Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn classify_recognises_contract_expired() {
        let event = contract_event(
            1,
            KEY_CONTRACT_EXPIRED,
            serde_json::json!({
                "ContractExpired": { "contract_id": "lct1abc", "refunded_to_poster": "5000000000" }
            }),
        );
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::ContractEvent { contract_id, kind } => {
                assert_eq!(contract_id, "lct1abc");
                assert_eq!(kind, ContractEventKind::Expired);
            }
            other => panic!("expected ContractEvent::Expired, got {other:?}"),
        }
    }

    #[test]
    fn contract_event_wins_over_bank_escrow_transfer() {
        // A PostContract tx emits BOTH a Contracts/ContractPosted
        // (semantic) AND a Bank/TokenTransferred for the escrow deposit.
        // The parser must pick the contract event, not the escrow
        // transfer.
        let semantic = contract_event(
            1,
            KEY_CONTRACT_POSTED,
            serde_json::json!({
                "ContractPosted": {
                    "contract_id": "lct1abc",
                    "poster": "lig1poster",
                    "arbiter": "lig1arbiter",
                    "pool": "5000000000"
                }
            }),
        );
        let escrow = LedgerEvent {
            r#type: "event".into(),
            number: 2,
            key: KEY_BANK_TOKEN_TRANSFERRED.into(),
            value: serde_json::json!({
                "token_transferred": {
                    "from": {"user": "lig1poster"},
                    "to":   {"user": "lig1escrow"},
                    "coins": {"amount": "5000000000", "token_id": "token_1lgt"}
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Bank".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&semantic, &escrow]).expect("not skipped");
        assert!(matches!(
            classified.kind,
            IndexerTx::ContractEvent {
                kind: ContractEventKind::Posted,
                ..
            }
        ));
    }

    /// The singular `Contract/` prefix (a plausible-but-wrong guess)
    /// must NOT classify as a contract event — it falls through to
    /// `Unknown`. This pins the plural-vs-singular gotcha as a
    /// regression test: if someone "fixes" the consts to `Contract/`,
    /// this fails.
    #[test]
    fn singular_contract_prefix_does_not_match() {
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "Contract/ContractPosted".into(), // WRONG (singular)
            value: serde_json::json!({
                "ContractPosted": {
                    "contract_id": "lct1abc",
                    "poster": "lig1poster",
                    "arbiter": "lig1arbiter",
                    "pool": "5000000000"
                }
            }),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Contract".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(),
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::Unknown { event_keys } => {
                assert_eq!(event_keys, vec!["Contract/ContractPosted"]);
            }
            other => panic!("expected Unknown for singular prefix, got {other:?}"),
        }
    }

    #[test]
    fn fixture_round_trip_from_chain_capture() {
        // Sanity-check that a real captured chain response deserialises
        // into our typed shapes without loss.
        const TX_FIXTURE: &str = include_str!("../tests/fixtures/tx-by-hash.json");
        const EVENT_FIXTURE: &str = include_str!("../tests/fixtures/tx-event-0.json");

        let tx: LedgerTx = serde_json::from_str(TX_FIXTURE).expect("tx fixture");
        let event: LedgerEvent = serde_json::from_str(EVENT_FIXTURE).expect("event fixture");

        assert_eq!(tx.r#type, "tx");
        assert_eq!(tx.hash, event.tx_hash);
        assert_eq!(tx.receipt.result, "successful");

        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::Transfer(t) => {
                assert_eq!(t.amount_nano, "1000000000");
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }
}
