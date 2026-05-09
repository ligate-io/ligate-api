-- ============================================================================
-- Migration 0002 — indexer query tables
-- ============================================================================
--
-- Adds the storage backing the v1 indexer query endpoints
-- (issues #1, #6). One table per resource exposed by RFC 0002:
--
--   transactions       — every chain-finalised tx, parsed by `kind`
--   attestor_sets      — registered M-of-N quorums (RegisterAttestorSet)
--   schemas            — registered attestation schemas (RegisterSchema)
--   attestations       — submitted attestations (SubmitAttestation)
--   address_summaries  — per-address denormalised counters
--
-- Encoding rules (per RFC 0002):
--   • u128 amounts:    NUMERIC(78, 0) ─ holds 2^128 with headroom
--   • Slot / nonce:    BIGINT (chain u64; Postgres-side i64 covers
--                       slot heights for ~2^63, well past chain lifetime)
--   • Hashes:          TEXT, lowercase hex with `0x` prefix
--   • Bech32m IDs:     TEXT, canonical lowercase form (`lig1`, `lsc1`,
--                       `las1`, `lph1`, `lpk1`, `token_1`)
--   • Timestamps:      TIMESTAMPTZ (UTC at write time)
--
-- Forward compatibility:
--   • `kind` on transactions is TEXT, with `'unknown'` reserved for
--     module/variant combinations the indexer can't yet parse. New
--     chain runtime calls don't break ingest — they just land as
--     `unknown` until a follow-up migration adds the typed shape.
--   • `details` is JSONB; per-`kind` shape pinned by RFC 0002.
--   • `raw` (JSONB) on transactions captures the full event payload
--     so deep-dive views can extract fields the typed columns don't
--     yet model.

-- ============================================================================
-- transactions
-- ============================================================================
--
-- One row per chain-finalised tx. Tx hash is the natural PK (chain
-- guarantees uniqueness via per-credential nonce + chain_hash binding).
-- (slot, position) is the keyset cursor sort key per RFC 0001.

CREATE TABLE IF NOT EXISTS transactions (
    -- Globally unique tx hash (32 bytes). Lowercase hex, `0x`-prefixed
    -- (66 chars total) to match RFC 0002's wire format.
    hash                  TEXT          PRIMARY KEY,

    -- Block this tx landed in.
    slot                  BIGINT        NOT NULL REFERENCES slots(height),
    -- Index within the block. Stable across reads; comes from the
    -- chain's per-slot tx ordering.
    position              INTEGER       NOT NULL,

    -- Authorisation. `sender` is the bech32m `lig1...` address derived
    -- from `pubkey[..28]` (per chain convention; NOT
    -- `SHA-256(pubkey)[..28]`).
    sender                TEXT          NOT NULL,
    sender_pubkey         TEXT          NOT NULL,
    nonce                 BIGINT        NOT NULL,

    -- Fee envelope. u128 stored as NUMERIC, serialised as decimal
    -- string at the API edge.
    fee_paid_nano         NUMERIC(78,0) NOT NULL,

    -- Tagged-union discriminator. Values:
    --   'transfer'
    --   'register_attestor_set'
    --   'register_schema'
    --   'submit_attestation'
    --   'unknown'    -- forward-compat for new runtime calls
    kind                  TEXT          NOT NULL,
    -- Per-`kind` payload. Shape pinned by RFC 0002 §"Tx kinds":
    --   transfer: { to, amount_nano, token_id }
    --   register_attestor_set: { attestor_set_id, members[], threshold }
    --   register_schema: { schema_id, name, version, attestor_set_id,
    --                      fee_routing_bps, fee_routing_addr,
    --                      payload_shape_hash }
    --   submit_attestation: { schema_id, payload_hash, signature_count }
    --   unknown: { raw_call_disc: [u8, u8] }
    details               JSONB         NOT NULL,

    -- Catch-all event payload. Lossy-typed by design; lets ingest keep
    -- pace with chain emits even when the typed columns lag.
    raw                   JSONB         NOT NULL,

    -- Outcome (chain-side, post-finality).
    --   'committed'  -- normal success
    --   'reverted'   -- fee was charged but state mutation failed
    outcome               TEXT          NOT NULL,
    revert_reason         TEXT,

    -- Bookkeeping.
    indexed_at            TIMESTAMPTZ   NOT NULL DEFAULT NOW(),

    -- Sanity constraints.
    CONSTRAINT transactions_kind_known CHECK (kind IN (
        'transfer',
        'register_attestor_set',
        'register_schema',
        'submit_attestation',
        'unknown'
    )),
    CONSTRAINT transactions_outcome_known CHECK (outcome IN (
        'committed',
        'reverted'
    )),
    -- `revert_reason` IFF outcome='reverted'.
    CONSTRAINT transactions_revert_reason_consistent CHECK (
        (outcome = 'reverted' AND revert_reason IS NOT NULL) OR
        (outcome = 'committed' AND revert_reason IS NULL)
    ),
    -- (slot, position) is unique within a chain — prevents accidental
    -- double-ingest if the indexer is restarted mid-slot.
    CONSTRAINT transactions_slot_position_unique UNIQUE (slot, position)
);

-- Cursor pagination for `/v1/txs` uses (slot DESC, position DESC).
-- The `(slot, position)` UNIQUE constraint above creates a composite
-- index that already serves this query path; an explicit DESC variant
-- isn't needed (Postgres reads ascending indexes backwards efficiently).

-- `/v1/addresses/{addr}` activity-history queries filter by sender.
CREATE INDEX IF NOT EXISTS transactions_sender_slot
    ON transactions (sender, slot DESC, position DESC);

-- Per-kind queries (e.g. all transfers in a slot range) and stats.
CREATE INDEX IF NOT EXISTS transactions_kind_slot
    ON transactions (kind, slot DESC);

-- Ops: "what did we ingest in the last hour".
CREATE INDEX IF NOT EXISTS transactions_indexed_at_brin
    ON transactions USING brin (indexed_at);


-- ============================================================================
-- attestor_sets
-- ============================================================================

CREATE TABLE IF NOT EXISTS attestor_sets (
    -- Bech32m `las1...` form. Deterministic id from
    -- `SHA-256(sorted(members) || threshold_u8)`.
    id                       TEXT         PRIMARY KEY,

    -- Members as JSONB array of bech32m `lpk1...` strings, ordered as
    -- the chain stored them (post-canonicalisation). Validation:
    --   • length 1..=64 (per chain `MAX_ATTESTOR_SET_MEMBERS`)
    --   • each entry is a 32-byte pubkey, bech32m-encoded
    members                  JSONB        NOT NULL,
    threshold                INTEGER      NOT NULL,

    -- Provenance: which tx registered this set.
    registered_at_slot       BIGINT       NOT NULL REFERENCES slots(height),
    registered_at_tx         TEXT         NOT NULL REFERENCES transactions(hash),
    registered_at_timestamp  TIMESTAMPTZ  NOT NULL,

    -- Denormalised counter, maintained by ingest. Incremented each
    -- time a `register_schema` tx binds to this set; never decremented
    -- (schemas are immutable once registered).
    schema_count             INTEGER      NOT NULL DEFAULT 0,

    indexed_at               TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    CONSTRAINT attestor_sets_threshold_range CHECK (threshold >= 1 AND threshold <= 64),
    CONSTRAINT attestor_sets_schema_count_nonneg CHECK (schema_count >= 0)
);

-- Cursor pagination for `/v1/attestor-sets` uses (registered_at_slot
-- DESC, id DESC). PK on `id` already covers id-keyed lookups; this
-- index covers the sort path.
CREATE INDEX IF NOT EXISTS attestor_sets_registered_at
    ON attestor_sets (registered_at_slot DESC, id DESC);

-- "Which attestor sets contain this pubkey?" — used by
-- `/v1/addresses/{addr}` to compute `attestor_member_count`.
-- JSONB GIN index on members supports `@> ['"lpk1..."']` queries.
CREATE INDEX IF NOT EXISTS attestor_sets_members_gin
    ON attestor_sets USING gin (members);


-- ============================================================================
-- schemas
-- ============================================================================

CREATE TABLE IF NOT EXISTS schemas (
    -- Bech32m `lsc1...` form. Deterministic id from
    -- `SHA-256(owner.as_ref() || name || version_le)`.
    id                       TEXT         PRIMARY KEY,

    name                     TEXT         NOT NULL,
    version                  INTEGER      NOT NULL,

    -- bech32m `lig1...` (28-byte address).
    owner                    TEXT         NOT NULL,
    -- Bound attestor set. NOT NULL because every schema must bind to
    -- a set at registration; FK enforces the binding even if a future
    -- chain change adds a "schemaless" mode.
    attestor_set_id          TEXT         NOT NULL REFERENCES attestor_sets(id),

    -- Fee routing (basis points, 0..=10000).
    fee_routing_bps          INTEGER      NOT NULL DEFAULT 0,
    fee_routing_addr         TEXT,                                    -- bech32m `lig1...`, NULL when bps=0
    -- SHA-256 of canonical schema-doc bytes. Lowercase hex, `0x`-prefixed.
    payload_shape_hash       TEXT         NOT NULL,

    registered_at_slot       BIGINT       NOT NULL REFERENCES slots(height),
    registered_at_tx         TEXT         NOT NULL REFERENCES transactions(hash),
    registered_at_timestamp  TIMESTAMPTZ  NOT NULL,

    -- Denormalised counter; incremented on each `submit_attestation`
    -- ingest where `details->>'schema_id' = id`. Never decremented.
    attestation_count        INTEGER      NOT NULL DEFAULT 0,

    indexed_at               TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    CONSTRAINT schemas_fee_routing_bps_range CHECK (fee_routing_bps >= 0 AND fee_routing_bps <= 10000),
    -- `fee_routing_addr` IFF `fee_routing_bps > 0`.
    CONSTRAINT schemas_fee_routing_addr_consistent CHECK (
        (fee_routing_bps > 0 AND fee_routing_addr IS NOT NULL) OR
        (fee_routing_bps = 0 AND fee_routing_addr IS NULL)
    ),
    CONSTRAINT schemas_attestation_count_nonneg CHECK (attestation_count >= 0),
    -- A given (owner, name, version) is unique on-chain (the deterministic
    -- id derivation guarantees this); enforce it in Postgres too.
    CONSTRAINT schemas_owner_name_version_unique UNIQUE (owner, name, version)
);

-- Cursor pagination for `/v1/schemas` uses (registered_at_slot DESC,
-- id DESC). PK on `id` covers single-resource lookups.
CREATE INDEX IF NOT EXISTS schemas_registered_at
    ON schemas (registered_at_slot DESC, id DESC);

-- "Schemas owned by this address" — used by
-- `/v1/addresses/{addr}.schemas_owned_count`.
CREATE INDEX IF NOT EXISTS schemas_owner
    ON schemas (owner);


-- ============================================================================
-- attestations
-- ============================================================================
--
-- One row per `submit_attestation` tx. The chain doesn't enforce
-- single-attestation-per-payload, so (schema_id, payload_hash) is NOT
-- unique on its own. The PK includes the source tx so re-submissions
-- (same payload, later tx) get distinct rows.
--
-- v0 endpoints don't query this table directly — `Schema.attestation_count`
-- is the only frontend-visible field driven by it. The table exists
-- now (instead of a later migration) so:
--   1. The counter has a source-of-truth that survives indexer rebuilds
--      (rebuild from chain replay → recompute counts from this table)
--   2. A future `GET /v1/schemas/{id}/attestations` endpoint just adds
--      a handler, no schema migration needed

CREATE TABLE IF NOT EXISTS attestations (
    schema_id                TEXT         NOT NULL REFERENCES schemas(id),
    -- Bech32m `lph1...`. Same payload can be attested under multiple
    -- schemas, hence not a global PK.
    payload_hash             TEXT         NOT NULL,
    submitter                TEXT         NOT NULL,
    submitter_pubkey         TEXT         NOT NULL,

    submitted_at_slot        BIGINT       NOT NULL REFERENCES slots(height),
    submitted_at_tx          TEXT         NOT NULL REFERENCES transactions(hash),
    submitted_at_timestamp   TIMESTAMPTZ  NOT NULL,

    -- Number of attestor signatures included with the submission.
    -- Always >= the bound schema's threshold (chain enforces).
    signature_count          INTEGER      NOT NULL,

    indexed_at               TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (schema_id, payload_hash, submitted_at_tx),
    CONSTRAINT attestations_signature_count_pos CHECK (signature_count >= 1)
);

-- "Attestations for this schema, newest first" — for the future
-- `GET /v1/schemas/{id}/attestations` list endpoint.
CREATE INDEX IF NOT EXISTS attestations_schema_recent
    ON attestations (schema_id, submitted_at_slot DESC, submitted_at_tx DESC);

-- "Attestations submitted by this address" — for the future
-- `GET /v1/addresses/{addr}/attestations` endpoint.
CREATE INDEX IF NOT EXISTS attestations_submitter
    ON attestations (submitter, submitted_at_slot DESC);


-- ============================================================================
-- address_summaries
-- ============================================================================
--
-- Denormalised per-address counters maintained transactionally by the
-- ingest pipeline. Live balances are NOT here — those come from the
-- chain via `LigateClient::getBalance` at handler time, since balances
-- can change in-slot via a tx the indexer hasn't yet ingested.
--
-- Fields tracked:
--   • txs_sent_count       — # txs where sender = address
--   • txs_received_count   — # transfers where details->>'to' = address
--   • first_seen_*         — earliest tx involving this address
--   • last_seen_*          — most recent tx involving this address
--   • schemas_owned_count  — # schemas where owner = address
--   • attestor_member_count — # attestor sets where members @> [pubkey]
--
-- The handler computes RFC 0002's `tx_count = sent + received` at read
-- time. Storing them separately (vs. a single counter) keeps the
-- ingest update logic simple — each tx insert increments the right
-- counter for the right role without disambiguation.

CREATE TABLE IF NOT EXISTS address_summaries (
    address                    TEXT         PRIMARY KEY,

    txs_sent_count             BIGINT       NOT NULL DEFAULT 0,
    txs_received_count         BIGINT       NOT NULL DEFAULT 0,

    first_seen_slot            BIGINT,
    first_seen_tx              TEXT,
    first_seen_timestamp       TIMESTAMPTZ,

    last_seen_slot             BIGINT,
    last_seen_tx               TEXT,
    last_seen_timestamp        TIMESTAMPTZ,

    schemas_owned_count        INTEGER      NOT NULL DEFAULT 0,
    attestor_member_count      INTEGER      NOT NULL DEFAULT 0,

    indexed_at                 TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    CONSTRAINT address_summaries_counts_nonneg CHECK (
        txs_sent_count        >= 0 AND
        txs_received_count    >= 0 AND
        schemas_owned_count   >= 0 AND
        attestor_member_count >= 0
    ),
    -- first_seen_* and last_seen_* either all set or all NULL (a row
    -- with the address present but no observed activity is invalid).
    CONSTRAINT address_summaries_first_seen_consistent CHECK (
        (first_seen_slot IS NULL AND first_seen_tx IS NULL AND first_seen_timestamp IS NULL) OR
        (first_seen_slot IS NOT NULL AND first_seen_tx IS NOT NULL AND first_seen_timestamp IS NOT NULL)
    ),
    CONSTRAINT address_summaries_last_seen_consistent CHECK (
        (last_seen_slot IS NULL AND last_seen_tx IS NULL AND last_seen_timestamp IS NULL) OR
        (last_seen_slot IS NOT NULL AND last_seen_tx IS NOT NULL AND last_seen_timestamp IS NOT NULL)
    )
);

-- "Addresses by recent activity" — diagnostic, not a public endpoint
-- yet but useful for ops and for any future leaderboard-y view.
CREATE INDEX IF NOT EXISTS address_summaries_last_seen
    ON address_summaries (last_seen_slot DESC NULLS LAST);
