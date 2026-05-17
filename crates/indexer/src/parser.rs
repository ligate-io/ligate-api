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
    AttestationSchemaRegisteredEvent, BankTokenTransferredEvent, LedgerEvent, LedgerTx,
};

/// Event-key constant for the Bank module's `TokenTransferred` event.
const KEY_BANK_TOKEN_TRANSFERRED: &str = "Bank/TokenTransferred";

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
    /// Catch-all. Either no events were emitted (e.g. a no-op tx), or
    /// the events present don't match any kind the parser knows. The
    /// indexer writes this as `kind = "unknown"` with the raw event
    /// keys captured in `details.event_keys` for forensic lookups.
    Unknown { event_keys: Vec<String> },
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
/// 1. `Bank/TokenTransferred` -> [`IndexerTx::Transfer`]
/// 2. `Attestation/AttestorSetRegistered` -> [`IndexerTx::RegisterAttestorSet`]
/// 3. `Attestation/SchemaRegistered`      -> [`IndexerTx::RegisterSchema`]
/// 4. `Attestation/AttestationSubmitted`  -> [`IndexerTx::SubmitAttestation`]
/// 5. otherwise -> [`IndexerTx::Unknown`] capturing the event keys
///    we saw, for forensic lookup
///
/// **Attestation events win over Bank events.** A `register_schema`
/// tx emits both an `Attestation/SchemaRegistered` (semantic) and a
/// `Bank/TokenTransferred` (the fee payment to treasury); without
/// this preference the parser would classify the tx as a Transfer
/// of the fee, dropping the semantic event. We walk events looking
/// for an Attestation-module key first; only if none is found do we
/// fall through to the bank check.
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

    // Pass 2: no semantic event matched; fall back to the bank check
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
