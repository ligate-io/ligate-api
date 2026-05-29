-- Work-for-hire contract indexer state. Mirrors the contract module's
-- chain state (ligate-chain contract primitive, v0.4.0+) so the api can
-- serve `/v1/contracts` list + detail without N chain RPC round-trips.
--
-- Populated by the indexer on `Contracts/ContractPosted`,
-- `WorkerCommitted`, `ContractDelivered`, `DeliveryAccepted`,
-- `DeliveryRejected`, `ContractDisputeResolved`, `ContractCancelled`,
-- `ContractExpired` events. The events are thin (id + addresses/amounts),
-- so the indexer hydrates the full record via the chain REST
-- (`GET /v1/modules/contract/contracts/{id}`) on every event and writes
-- it here; terminal transitions also zero `escrow_remaining_nano`.
--
-- **Event-key prefix gotcha.** The contract module's struct is named
-- `Contracts` (plural), so the SDK-derived event keys are
-- `Contracts/ContractPosted` etc. — NOT `Contract/...`. See
-- `crates/indexer/src/parser.rs` for the consts + the SDK derivation
-- proof.
--
-- See `docs/protocol/contract-primitive.md` in ligate-chain for the
-- on-chain shape this table mirrors. Unlike `bounties`, contracts are
-- NOT schema-anchored (the poster names a specific arbiter address at
-- post time), so there is no schema FK here; `arbiter` is a plain named
-- address column.

CREATE TABLE IF NOT EXISTS contracts (
    -- Bech32m `lct1...` form. Deterministic id from
    -- `SHA-256(poster.as_ref() || criteria_doc_hash || nonce_le)`.
    id                       TEXT         PRIMARY KEY,

    -- `lig1...` of the address that posted the contract (buyer).
    -- Receives refunds on cancel/expiry and bond payouts on
    -- rejected-dispute resolutions.
    poster                   TEXT         NOT NULL,

    -- `lig1...` of the arbiter named at post time. Authorised to call
    -- ResolveContractDispute. A named address, NOT a schema-derived
    -- role — hence no FK (contracts aren't schema-anchored).
    arbiter                  TEXT         NOT NULL,

    -- 32-byte content hash of the off-chain criteria document (test
    -- cases / acceptance rubric). Stored as TEXT verbatim from the
    -- chain's serialisation (hex today); same pass-through convention
    -- as `schemas.payload_shape_hash`.
    criteria_doc_hash        TEXT         NOT NULL,

    -- Total AVOW originally escrowed at PostContract time, in nanos.
    -- Kept as TEXT to preserve u128 precision over the Postgres
    -- int4/int8 gap (Amount is u128 on chain). Same rationale as
    -- `bounties.pool_nano`.
    pool_nano                TEXT         NOT NULL,

    -- Remaining escrow at the indexer's last seen event. Seeded to
    -- `pool_nano` on ContractPosted; reset to zero on the terminal
    -- transitions (accepted / dispute-resolved / cancelled / expired)
    -- after the chain drains the module escrow (payout or refund).
    -- Contract escrow is all-or-nothing — no per-event decrement like
    -- bounty claims.
    escrow_remaining_nano    TEXT         NOT NULL,

    -- Arbiter fee in basis points (paid only if the arbiter resolves a
    -- dispute). Chain caps at 5000 (50%); default 500 (5%).
    arbiter_fee_bps          INTEGER      NOT NULL,

    -- One of the 8 contract lifecycle states (lowercased from the
    -- chain's PascalCase `ContractStatus`). Lifecycle:
    --   open       posted, accepting commits
    --   committed  a worker has committed (and possibly delivered)
    --   delivered  worker submitted delivery; awaiting acceptance window
    --   accepted   poster (or auto-accept) accepted; payout settled
    --   rejected   poster rejected; dispute resolved against the worker
    --   disputed   in dispute; arbiter resolving
    --   cancelled  poster cancelled an Open contract (no commits)
    --   expired    expiry passed before delivery; pool refunded
    status                   TEXT         NOT NULL CHECK (status IN
                                            ('open', 'committed', 'delivered',
                                             'accepted', 'rejected', 'disputed',
                                             'cancelled', 'expired')),

    -- DA-layer block height the contract expires at.
    expiry_da_height         BIGINT       NOT NULL,

    -- Window in chain blocks the poster has to accept-or-reject a
    -- delivery before it auto-accepts (FinalizeDelivery sweep). Used by
    -- the API to render "X blocks remaining in acceptance window" hints.
    dispute_window_blocks    INTEGER      NOT NULL,

    -- Provenance: slot the PostContract tx landed in. Drives the
    -- default `(posted_at_slot DESC, id DESC)` ordering on list queries.
    posted_at_slot           BIGINT       NOT NULL,
    posted_at_tx             TEXT         NOT NULL,
    posted_at_timestamp      TIMESTAMPTZ  NOT NULL
);

-- Default list ordering: `(posted_at_slot DESC, id DESC)`.
CREATE INDEX IF NOT EXISTS contracts_posted_slot_id_idx
    ON contracts (posted_at_slot DESC, id DESC);

-- Status filter: `/v1/contracts?status=open` is the buyer/worker
-- dashboard's hot path. Partial-free (all 8 states are queried), so a
-- plain b-tree on status backs the `($N IS NULL OR status = $N)` guard.
CREATE INDEX IF NOT EXISTS contracts_status_idx
    ON contracts (status);

-- Poster lookups: "what contracts did this address post?" Mirrors
-- `bounties_poster_idx`.
CREATE INDEX IF NOT EXISTS contracts_poster_idx
    ON contracts (poster);

-- Arbiter lookups: "what contracts am I the named arbiter for?" backs
-- the `?arbiter=` filter (arbiter dashboard's dispute queue).
CREATE INDEX IF NOT EXISTS contracts_arbiter_idx
    ON contracts (arbiter);
