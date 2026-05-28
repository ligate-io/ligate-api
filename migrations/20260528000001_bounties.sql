-- Bounty marketplace indexer state. Mirrors the bounty module's chain
-- state (chain#519, ligate-chain v0.4.0+) so the matching service can
-- answer "what open bounties is this address eligible for" in one
-- Postgres join instead of N chain RPC round-trips.
--
-- Populated by the indexer on `Bounty/BountyPosted`, `BountyClaimed`,
-- `BountyDisputed`, `DisputeResolved`, `BountyExpired` events.
-- v0 ingestion focuses on `BountyPosted` (board schema id, pool,
-- per_attestation, acceptance, expiry, status='open'); the lifecycle
-- transitions (claim/dispute/cancel) flip the `status` column and
-- adjust `escrow_remaining`.
--
-- See `docs/protocol/bounty-marketplace.md` in ligate-chain for the
-- on-chain shape this table mirrors.

CREATE TABLE IF NOT EXISTS bounties (
    -- Bech32m `lbt1...` form. Deterministic id from
    -- `SHA-256(poster.as_ref() || board_schema_id || nonce_le)`.
    id                       TEXT         PRIMARY KEY,

    -- `lid1...` of the address that posted the bounty. Receives
    -- escrow refunds on cancel and rejected-dispute bond payouts.
    poster                   TEXT         NOT NULL,

    -- `lsc1...` of the bounty board schema this bounty composes
    -- against. References `schemas(id)`; the matching service joins
    -- through this to find attestations a candidate has submitted.
    board_schema_id          TEXT         NOT NULL REFERENCES schemas(id),

    -- Original pool size at PostBounty time, in AVOW nanos. Kept as
    -- a TEXT to preserve u128 precision over the Postgres int4/int8
    -- gap (Amount is u128 on chain). Numeric ordering still works
    -- via length-then-lex thanks to the leading-zero-pad on
    -- formatting; see `format_amount_nanos` in `crates/api/src/queries.rs`.
    pool_nano                TEXT         NOT NULL,

    -- AVOW paid out per accepted claim, in nanos. Same TEXT
    -- precision rationale as `pool_nano`.
    per_attestation_nano     TEXT         NOT NULL,

    -- Remaining escrow at the indexer's last seen event for this
    -- bounty. Decremented on `BountyClaimed`; reset to zero on
    -- `BountyExpired` / `BountyCancelled` after refund.
    escrow_remaining_nano    TEXT         NOT NULL,

    -- One of {'open', 'exhausted', 'expired', 'cancelled', 'finalised'}.
    -- The matching service filters on `status='open'` to surface only
    -- payable bounties. Lifecycle:
    --   open       posted, payable
    --   exhausted  escrow < per_attestation after claims; no more payouts
    --   expired    `expiry_da_height` passed without full payout
    --   cancelled  poster called CancelBounty
    --   finalised  all claims paid + dispute window closed (v1 status)
    status                   TEXT         NOT NULL CHECK (status IN
                                            ('open', 'exhausted', 'expired',
                                             'cancelled', 'finalised')),

    -- Acceptance predicate as compact JSONB. Mirrors the chain's
    -- `AcceptancePredicate` enum. Four shapes:
    --   {"any": {}}
    --   {"attestor_set": {"id": "las1..."}}
    --   {"payload_hashes": {"hashes": ["lph1...", "lph1..."]}}
    --   {"peer_count": {"min_attestors": N}}
    -- v1 `All(SafeVec<Self>)` lands as `{"all": [...]}` without
    -- schema change.
    acceptance               JSONB        NOT NULL,

    -- DA-layer block height the bounty expires at. NULL if not yet
    -- decoded (e.g. v1 chain event extension); v0 always sets.
    expiry_da_height         BIGINT       NOT NULL,

    -- Dispute window in chain blocks; used by the API to render
    -- "X blocks remaining in dispute window" hints.
    dispute_window_blocks    INTEGER      NOT NULL,

    -- Provenance: slot the PostBounty tx landed in. Drives the
    -- default `(slot DESC, id DESC)` ordering on list queries.
    posted_at_slot           BIGINT       NOT NULL,
    posted_at_tx             TEXT         NOT NULL,
    posted_at_timestamp      TIMESTAMPTZ  NOT NULL,

    -- Running count of `BountyClaimed` events seen against this
    -- bounty. Surfaced in list responses so partners can see "this
    -- bounty has paid out N times" without an N+1 fetch.
    claim_count              INTEGER      NOT NULL DEFAULT 0,

    -- Set on the first `BountyClaimed`; lets the API answer "when
    -- was this bounty last touched" without a separate join.
    last_claim_at_slot       BIGINT
);

-- Status-first index: the matching service's hot path is
-- `WHERE status = 'open' AND board_schema_id = ANY (...)`.
CREATE INDEX IF NOT EXISTS bounties_status_schema_idx
    ON bounties (status, board_schema_id)
    WHERE status = 'open';

-- Default list ordering: `(posted_at_slot DESC, id DESC)`.
CREATE INDEX IF NOT EXISTS bounties_posted_slot_id_idx
    ON bounties (posted_at_slot DESC, id DESC);

-- Poster lookups: "what bounties did this address post?" is the
-- buyer dashboard's primary query. Same shape as the
-- `attestations_submitter_idx` on the attestations table.
CREATE INDEX IF NOT EXISTS bounties_poster_idx
    ON bounties (poster);
