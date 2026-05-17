-- ============================================================================
-- 20260517000001_attestation_id_lat.sql
--
-- v0.2.0 wire-format change (ligate-chain#381): AttestationId collapses
-- from the compound `(schema_id, payload_hash)` form (rendered as
-- `lsc1...:lph1...`) to a single 32-byte bech32m hash with the `lat`
-- HRP (`lat1...`), derived deterministically as
--
--     AttestationId = SHA-256(schema_id_bytes || payload_hash_bytes)
--
-- where each side is the raw 32-byte underlying hash. The chain
-- reference impl lives at
-- `ligate-chain/crates/modules/attestation/src/lib.rs::AttestationId::from_pair`.
--
-- This migration is a destructive schema change. Pre-v0.2.0 rows
-- stored the compound form; backfilling a `lat1...` id in pure SQL
-- isn't possible (Postgres has no SHA-256 + bech32 helpers), so the
-- table is TRUNCATEd on the assumption that the operator has run a
-- devnet re-genesis (separate operator runbook) before applying.
-- The indexer re-populates from chain history on its next ingest
-- pass after this migration lands.
--
-- The new `id` column is UNIQUE because the chain invariant
-- guarantees one attestation per `(schema_id, payload_hash)` pair,
-- and the id is a pure function of that pair. UPSERTs in the indexer
-- now target this constraint, which also folds repeat submissions of
-- the same logical attestation into a single row.
-- ============================================================================

TRUNCATE TABLE attestations;

ALTER TABLE attestations
    ADD COLUMN id TEXT NOT NULL;

CREATE UNIQUE INDEX attestations_id_unique
    ON attestations(id);
