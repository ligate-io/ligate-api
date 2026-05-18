-- slots.da_block_height — Celestia (DA) block height where this slot's
-- batch blob was included. Powers explorer-side "View on Celenium"
-- deep-links per ligate-io/ligate-chain#355.
--
-- Nullable for two reasons:
--   1. Historical rows ingested before this migration land here with
--      NULL until an optional one-shot backfill re-fetches each slot's
--      first batch and updates the column.
--   2. Slots whose first batch fetch fails (404 / transient RPC error)
--      keep NULL even after the migration. The slot row itself still
--      lands; only the deep-link is unavailable.
--
-- BIGINT (Postgres i64) is the right shape: Celestia heights are u64
-- but a chain producing a block per second wouldn't hit i64::MAX
-- for ~292 billion years. The indexer-side bind goes through i64 for
-- sqlx ergonomics; same convention as the existing `slot.height`
-- column.

ALTER TABLE slots ADD COLUMN IF NOT EXISTS da_block_height BIGINT;
