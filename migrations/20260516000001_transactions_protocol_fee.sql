-- ============================================================================
-- Migration 0005 — `transactions.protocol_fee_nano` column
-- ============================================================================
--
-- The chain charges two distinct fees on every tx and they belong in
-- different columns:
--
--   GAS fee (`fee_paid_nano`):       chain-metered execution cost.
--                                    Currently 0 on devnet (`gas_price = 0`
--                                    in genesis); will be non-zero once
--                                    gas pricing activates on testnet+.
--                                    Already a column on `transactions`.
--
--   PROTOCOL fee (`protocol_fee_nano`): flat per-call-type module fee.
--                                    Charged regardless of `gas_price`.
--                                    For attestation calls on ligate-devnet-1
--                                    (`chain/devnet-1/genesis/attestation.json`):
--                                      register_attestor_set  = 50_000_000  nano (0.05  LGT)
--                                      register_schema        = 100_000_000 nano (0.10  LGT)
--                                      submit_attestation     = 100_000     nano (0.0001 LGT)
--                                    Routes to treasury (default) or to a
--                                    builder share per schema config.
--                                    Note: these are devnet values; testnet/mainnet
--                                    will be substantially higher per governance.
--                                    For bank.transfer:       = 0 (no protocol fee).
--                                    For unknown kinds:       = NULL (haven't seen).
--
-- Until this column existed, the api's `/v1/txs/{hash}` and
-- `/v1/stats/totals` reported `fee_paid_nano: "0"` for an attestation
-- tx that actually cost its submitter 100 LGT in protocol fees,
-- silently mis-representing tx cost. The indexer's parser extracts
-- this from the Bank/TokenTransferred event(s) the chain emits
-- alongside the attestation event (the fee transfer is a separate
-- bank-module emission).
--
-- Nullable because:
--   - some tx kinds genuinely have no protocol fee (bank.transfer)
--   - unknown-kind txs we haven't taught the indexer to extract for
--   - historical rows pre-this-migration get `NULL` on the backfill
--     (re-ingest via cursor reset would fix forward; not done as
--     part of this migration to keep migrations idempotent)
--
-- Stored as NUMERIC(78,0) for the same reason as `fee_paid_nano`:
-- u128 representable as a decimal string for the wire layer.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS protocol_fee_nano NUMERIC(78, 0);

-- Backfill heuristic: for already-indexed rows where we KNOW the
-- protocol fee (devnet-1 genesis constants from
-- `chain/devnet-1/genesis/attestation.json`), seed the column so
-- the explorer renders historically accurate values without a
-- re-ingest. Values:
--   register_attestor_set:    50_000_000      (0.05 LGT, 9 decimals)
--   register_schema:         100_000_000      (0.10 LGT)
--   submit_attestation:          100_000      (0.0001 LGT)
--   transfer:                          0      (no protocol fee)
--
-- For kinds that vary by config at runtime (schema's
-- `fee_routing_bps` splits SubmitAttestation between treasury and
-- builder) the indexer will write the correct sum-from-events value
-- on next ingest; this backfill matches what would have landed if
-- every schema used `fee_routing_bps = 0` (the only schema we have
-- right now does). Re-ingest overrides via ON CONFLICT at write time.
UPDATE transactions
SET protocol_fee_nano = CASE kind
    WHEN 'register_attestor_set' THEN 50000000
    WHEN 'register_schema'       THEN 100000000
    WHEN 'submit_attestation'    THEN 100000
    WHEN 'transfer'              THEN 0
    ELSE NULL
END
WHERE protocol_fee_nano IS NULL;
