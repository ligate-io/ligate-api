-- ============================================================================
-- Migration 0007 — backfill `transactions.fee_paid_nano` from NULL to 0
-- ============================================================================
--
-- Devnet-1 reality (verified live on 2026-05-16): the chain meters gas
-- but doesn't bill — `gas_used = [0, 0]` on every batch receipt observed,
-- though `gas_price` is non-zero at `[7, 7]`. So per-tx gas fee paid is
-- actually 0, not unknown.
--
-- Until this migration:
--   - Indexer wrote `fee_paid_nano = NULL` on every tx insert
--   - Explorer rendered "fee not exposed yet" on every tx detail page
--   - Couldn't distinguish "0 LGT (real)" from "unknown (not yet
--     surfaced)" in the wire format
--
-- This migration:
--   - Backfills `fee_paid_nano = 0` for all existing rows where it's NULL
--   - Indexer code (crates/indexer/src/db.rs::insert_transaction) is
--     updated in the same PR to write 0 on every new insert + on
--     ON CONFLICT updates
--
-- Surfaced as Tier 3.1 of the explorer perf brief at ligate-api#48.
--
-- Future:
--   - On testnet/mainnet, gas pricing will actually bill (`gas_used` no
--     longer 0). At that point indexer should extract per-tx gas_used
--     from the chain batch receipt and compute `gas_used * gas_price`
--     for the real fee. Tracked as a follow-up.
--   - The wire-format docstring in queries.rs (per api#47) explains
--     the gas_used=0 vs gas_price>0 split — keep that doc in sync.

UPDATE transactions
SET fee_paid_nano = 0
WHERE fee_paid_nano IS NULL;
