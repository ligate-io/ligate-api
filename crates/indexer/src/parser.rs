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

use ligate_api_types::{BankTokenTransferredEvent, LedgerEvent, LedgerTx};

/// Event-key constant for the Bank module's `TokenTransferred` event.
const KEY_BANK_TOKEN_TRANSFERRED: &str = "Bank/TokenTransferred";

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

/// Classify a `LedgerTx` plus its emitted events into an [`IndexerTx`].
///
/// Returns `None` if the tx was [`TxOutcome::Skipped`] — the caller
/// should not persist anything in that case.
///
/// `events` should be every `LedgerEvent` whose `tx_hash` matches
/// `tx.hash`. Caller is responsible for the filter (typically: fetch
/// `/v1/ledger/slots/{n}/events` once, group by `tx_hash`).
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
/// 2. otherwise -> [`IndexerTx::Unknown`] capturing the event keys
///    we saw, for forensic lookup
fn classify_events(events: &[&LedgerEvent]) -> IndexerTx {
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
    use super::*;
    use ligate_api_types::{FullyBakedTx, TxReceipt, Uint64Range};

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
            IndexerTx::Unknown { .. } => panic!("expected Transfer, got Unknown"),
        }
    }

    #[test]
    fn classify_falls_back_to_unknown_for_unrecognised_events() {
        let event = LedgerEvent {
            r#type: "event".into(),
            number: 1,
            key: "Attestation/AttestorSetRegistered".into(),
            value: serde_json::json!({"attestor_set_registered": {}}),
            module: ligate_api_types::ModuleRef {
                r#type: "moduleRef".into(),
                name: "Attestation".into(),
            },
            tx_hash: "ltx1deadbeef0000000000000000000000000000000000000000000000000".into(), // synthetic bech32m fixture, format-opaque to parser
        };
        let tx = fixture_tx("successful");
        let classified = classify_tx(&tx, &[&event]).expect("not skipped");
        match classified.kind {
            IndexerTx::Unknown { event_keys } => {
                assert_eq!(event_keys, vec!["Attestation/AttestorSetRegistered"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
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
            IndexerTx::Unknown { event_keys } => {
                panic!("expected Transfer, got Unknown with keys {event_keys:?}")
            }
        }
    }
}
