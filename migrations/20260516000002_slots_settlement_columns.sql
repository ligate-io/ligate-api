-- ============================================================================
-- Migration 0006 — slots settlement columns + prev_hash backfill
-- ============================================================================
--
-- This migration fills in three explorer-visible block fields that were
-- silently NULL before, and adds two new ones for DA-finality semantics:
--
--   prev_hash       (existing column, never populated)
--   proposer        (existing column on schema; populated for first time)
--   finality_status (NEW: 'pending' | 'finalized')
--   finalized_at    (NEW: observed instant of pending→finalized flip)
--
-- Why each was missing:
--
--   prev_hash:        the chain's `GET /v1/ledger/slots/{n}` JSON does not
--                     emit a prev_hash field at all. Verified live across
--                     slots 6222, 6257, 6264, 6267 on rpc.ligate.io —
--                     none had the field. Indexer's `SlotResponse` has
--                     `#[serde(default)] prev_hash`, so the absent field
--                     silently became NULL → BlockResponse.parent_hash
--                     was always null → explorer rendered empty.
--                     Fix: derive in the indexer from slot N-1's hash,
--                     backfilled here so historical rows render too.
--
--   proposer:         the chain doesn't expose a rollup-native sequencer
--                     identity in the slot JSON (`ligate-chain#82` tracks
--                     proper leader rotation). But each batch carries
--                     `receipt.da_address` — the Celestia wallet that
--                     submitted the blob to DA — which IS the sequencer's
--                     identity. The indexer already fetches every batch
--                     during ingest, so adding this is zero extra HTTP.
--                     Fix: bind from `batch.receipt.da_address` of the
--                     slot's first batch. Historical rows get a one-shot
--                     backfill below from any cached batch data.
--                     (For devnet-1 there's a single sequencer; expect
--                     identical da_address across all rows. That's fine.)
--
--   finality_status:  chain JSON DOES expose this — `"pending"` for the
--                     newest 1-3 slots, `"finalized"` once Celestia
--                     inclusion is confirmed. Indexer wasn't reading it.
--                     Fix: add the column, indexer reads + writes on
--                     every ingest. Backfill assumes 'finalized' for any
--                     row older than 5 slots from head (15-30s on Mocha,
--                     finalization comfortably done).
--
--   finalized_at:     wall-clock instant the indexer observed the flip
--                     from pending → finalized. Approximation of true
--                     finalization time (true value would come from
--                     chain's BlobExecutionStatus broadcast or from
--                     Celestia header timestamp; both deferred). For
--                     `/v1/stats/finality` switching from `source =
--                     "estimated"` to `source = "observed"` this is good
--                     enough: `finalized_at - slots.timestamp` gives a
--                     real per-slot finalization-latency distribution
--                     within ~indexer-poll-interval of truth.
--                     Backfill: NULL for historical rows (we don't have
--                     the observation). Going forward the indexer's
--                     re-poll loop stamps it.

-- ----------------------------------------------------------------------------
-- Schema changes
-- ----------------------------------------------------------------------------

-- `slots.proposer` was reserved on `BlockResponse` for leader rotation
-- but never made it onto the table. Adding it now; column type matches
-- `slots.hash` (TEXT, opaque to the indexer — `celestia1...` bech32 for
-- v0).
ALTER TABLE slots
    ADD COLUMN IF NOT EXISTS proposer TEXT;

-- Tagged-string enum: `'pending'` | `'finalized'`. Nullable for legacy
-- rows the backfill below couldn't infer (head-N slots at migration
-- time). Frontend treats NULL as "unknown" and renders no badge.
ALTER TABLE slots
    ADD COLUMN IF NOT EXISTS finality_status TEXT;

ALTER TABLE slots
    ADD COLUMN IF NOT EXISTS finalized_at TIMESTAMPTZ;

-- Partial index for the re-poll loop. The indexer scans
-- `WHERE finality_status = 'pending'` every ~10s and re-fetches each
-- row's chain JSON to detect the flip. A full-table sequential scan
-- gets expensive once `slots` is north of ~10k rows; this index keeps
-- the lookup O(pending-count), which is bounded at ~3-5 rows on Mocha.
CREATE INDEX IF NOT EXISTS slots_finality_pending_idx
    ON slots (finality_status)
    WHERE finality_status = 'pending';

-- Index for `/v1/stats/finality` observed sampling: percentile_cont
-- over the last hour's `finalized_at - timestamp` deltas. The WHERE
-- clause excludes legacy NULL rows. Without this index that stats
-- query scans the table; with it, the planner walks the index
-- forward from `NOW() - 1 hour`.
CREATE INDEX IF NOT EXISTS slots_finalized_at_idx
    ON slots (finalized_at)
    WHERE finalized_at IS NOT NULL;

-- ----------------------------------------------------------------------------
-- Backfill: prev_hash
-- ----------------------------------------------------------------------------
--
-- Slot N's prev_hash is slot N-1's hash. A correlated subquery here
-- runs O(N rows) with a primary-key lookup per row — fine for the
-- ~6k slots currently indexed. Idempotent: only fills where the
-- target is currently NULL.

UPDATE slots s
SET prev_hash = (
    SELECT hash FROM slots p WHERE p.height = s.height - 1
)
WHERE s.prev_hash IS NULL
  AND s.height > 0;

-- ----------------------------------------------------------------------------
-- Backfill: finality_status
-- ----------------------------------------------------------------------------
--
-- Anything older than `head - 5` is overwhelmingly likely to be
-- finalized on Mocha (3-block confirmation depth + safety margin).
-- Indexer-on-next-poll will correct any wrongly-tagged row by
-- re-reading the chain JSON, so a stale 'finalized' on a row that's
-- actually still 'pending' would get overwritten within ~10s.
--
-- Rows in the head-5 window get NULL — the indexer's first read after
-- deploy fills them from chain.

UPDATE slots s
SET finality_status = 'finalized'
WHERE s.finality_status IS NULL
  AND s.height < (SELECT MAX(height) - 5 FROM slots);

-- ----------------------------------------------------------------------------
-- Backfill: proposer (best-effort from raw JSONB)
-- ----------------------------------------------------------------------------
--
-- The indexer cached the full slot JSON in `slots.raw` but `raw`
-- doesn't contain the batch's da_address — that's on the batch, not
-- the slot. So this backfill has nothing local to read from.
--
-- Two options:
--   (a) leave NULL; indexer fills going forward as new slots ingest
--       (historical rows stay NULL until a cursor reset).
--   (b) re-ingest historical slots to populate.
--
-- Going with (a) for migration idempotency; ops can trigger (b) by
-- resetting the indexer cursor if historical proposer data is needed
-- on the explorer. For devnet-1 (single sequencer) all rows would get
-- the same value anyway.
--
-- No-op statement below to make the intent explicit:
-- (slots.proposer remains NULL until indexer rewrites on next ingest)

-- ----------------------------------------------------------------------------
-- finalized_at: no backfill (intentional)
-- ----------------------------------------------------------------------------
--
-- `finalized_at` is an observation (indexer stamping NOW() at the
-- moment it sees pending→finalized). We don't have that observation
-- for historical rows, and the chain JSON doesn't carry the true
-- finalization timestamp. Per /v1/stats/finality docstring, the
-- "observed" sampling window excludes NULL rows so legacy data
-- doesn't pollute the percentiles.
