-- Initial schema for ligate-indexer.
--
-- Two surfaces in v0: a `slots` table that mirrors the chain's
-- per-slot summary (height, timestamp, tx count, hash), and a
-- key-value `indexer_state` table for chain-identity bootstrap and
-- backfill cursors. Future migrations will add `transactions`,
-- `schemas`, `attestor_sets`, `attestations` as the chain modules
-- they consume stabilize.

-- ============================================================================
-- slots
-- ============================================================================

CREATE TABLE IF NOT EXISTS slots (
    height       BIGINT       PRIMARY KEY,
    hash         TEXT         NOT NULL,
    prev_hash    TEXT,
    state_root   TEXT,
    timestamp    TIMESTAMPTZ  NOT NULL,
    batch_count  INTEGER      NOT NULL DEFAULT 0,
    tx_count     INTEGER      NOT NULL DEFAULT 0,
    -- Catch-all for fields the typed shape doesn't model yet. Lossy-
    -- typed by design so an SDK rev that adds a slot field doesn't
    -- break ingest. Frontend reads typed columns; deep-dive views
    -- can JSON-extract from `raw` until a typed column is added.
    raw          JSONB        NOT NULL,
    -- Bookkeeping.
    indexed_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- The frontend's `getLatestBlocks` query orders by height DESC. The
-- PRIMARY KEY index already covers descending lookups, but an explicit
-- BRIN on indexed_at helps "what did we ingest in the last hour"
-- queries that we'll want for ops dashboards.
CREATE INDEX IF NOT EXISTS slots_indexed_at_brin
    ON slots USING brin (indexed_at);

-- ============================================================================
-- indexer_state
-- ============================================================================
--
-- Simple k/v table so the frontend can render chain identity badges
-- (chain_id, chain_hash, version) without making its own HTTP call to
-- the node. The indexer writes these on startup from /v1/rollup/info.
-- Also used as the backfill cursor (key = `last_indexed_height`) so a
-- restart resumes where it left off.

CREATE TABLE IF NOT EXISTS indexer_state (
    k          TEXT         PRIMARY KEY,
    v          TEXT         NOT NULL,
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
