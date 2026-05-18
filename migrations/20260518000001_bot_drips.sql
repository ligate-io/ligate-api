-- Discord faucet bot drips: independent ledger from the web faucet.
--
-- The web faucet (`POST /v1/drip`) keeps per-address cooldowns in-process
-- (DashMap in `ligate-api-drip`'s `RateLimiter`). That's fine for a 24h
-- window because a process restart "only" loses the last 24h of cooldown
-- state, and Railway restarts are rare.
--
-- The Discord faucet (`POST /v1/drip-bot`) runs on a 5-day cooldown with
-- tier-aware amounts (100/250/500/1000 LGT). At 5 days, an in-process
-- map would lose multiple-day cooldowns on every restart, which is a
-- real abuse window. So we persist bot drips to Postgres.
--
-- Two cooldown checks fire on every `/v1/drip-bot` call, against the
-- same table:
--   1. per-address: did any user drip to `lig1...` in the last 5 days?
--   2. per-discord-user: did this Discord user drip ANY address in 5d?
--
-- Both indexes below are unconditional B-tree on `dripped_at DESC` so
-- the cooldown query is a single index seek (no filter, no scan).

CREATE TABLE bot_drips (
    id BIGSERIAL PRIMARY KEY,

    -- The chain address that received the drip. bech32m `lig1...`.
    -- Cooldown lookup key #1: "last drip to THIS address".
    address TEXT NOT NULL,

    -- The Discord user who initiated the drip. Snowflake string;
    -- Discord IDs are 64-bit but we store as TEXT to avoid integer
    -- precision pitfalls in the bot ↔ api JSON round-trip.
    -- Cooldown lookup key #2: "last drip BY this user".
    discord_user_id TEXT NOT NULL,

    -- Amount dripped, in nano-LGT. u128 fits in NUMERIC(39, 0); we use
    -- NUMERIC because Postgres has no native u128 / unsigned types and
    -- bigint (i64) overflows at ~9.2 nano-LGT supply — fine today,
    -- not OK long-term.
    amount_nano NUMERIC(39, 0) NOT NULL,

    -- Chain tx hash from the signer's submission. bech32m `ltx1...`
    -- as of chain v0.2.x.
    tx_hash TEXT NOT NULL,

    -- Tier the bot computed locally and the api re-validated. One of
    -- 'newcomer' | 'regular' | 'veteran' | 'elder'. Stored as TEXT (not
    -- enum) so future tiers don't need an `ALTER TYPE`.
    tier TEXT NOT NULL,

    -- Snapshots of the tier inputs at drip time. Pure analytics — not
    -- read by the cooldown check. Useful for tuning tier boundaries
    -- later ("how many drips would each cohort have received under a
    -- different curve?").
    server_tenure_days INT,
    account_age_days INT,

    -- Server timestamp (NOT chain timestamp). The cooldown check
    -- compares against `NOW() - INTERVAL '<bot_drip_rate_limit_secs>'`
    -- so a chain pause wouldn't accidentally fast-forward cooldowns.
    dripped_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Cooldown index #1: per-address. The query is
--   SELECT 1 FROM bot_drips WHERE address = $1
--   AND dripped_at > NOW() - INTERVAL '5 days' LIMIT 1
-- DESC sort lets the planner read newest first and short-circuit.
CREATE INDEX bot_drips_address_dripped_at_idx
    ON bot_drips (address, dripped_at DESC);

-- Cooldown index #2: per-Discord-user. Mirror of #1.
CREATE INDEX bot_drips_discord_user_dripped_at_idx
    ON bot_drips (discord_user_id, dripped_at DESC);
