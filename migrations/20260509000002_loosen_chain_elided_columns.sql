-- ============================================================================
-- Migration 0003 — make chain-elided columns NULL-able on `transactions`
-- ============================================================================
--
-- Migration 0002 declared `sender_pubkey`, `nonce`, and `fee_paid_nano`
-- as `NOT NULL` on the `transactions` table, on the assumption that
-- the chain would expose them in tx JSON. After verifying against a
-- live localnet (chain `ligate-localnet`, slot 8975, original capture
-- pre-dated the bech32m chain output; today the same tx would be
-- `ltx19zwttsdksue0ef4fan7lnfhcjdq9lq8d592hjpcc30gh5c77ytzqvjmjm4`),
-- we now know:
--
--   • `LedgerTx.body.data` is the empty string `""` in chain JSON
--     responses (the chain elides borsh-encoded body bytes from the
--     public RPC). Sender pubkey + nonce + fee envelope all live
--     INSIDE the body.
--
--   • Address-derived fields ARE available via emitted events
--     (`Bank/TokenTransferred.from.user` is the bech32m sender
--     address, derived from `pubkey[..28]`). But the FULL 32-byte
--     pubkey isn't exposed.
--
--   • Gas usage is in `LedgerTx.receipt.data.gas_used: [u64; 2]` but
--     the user-visible "fee paid" is `gas_used * gas_price` summed
--     across both gas dimensions; we'd be computing it, not reading
--     it. Better to surface what the chain emits (gas_used) and let
--     consumers decide.
--
-- This migration loosens the constraints so the indexer can record
-- what it actually has. RFC 0002 surfaces the same fields with
-- nullable typing in the JSON layer.
--
-- A future migration can re-tighten if the chain starts exposing the
-- elided fields. Adding a column back to NOT NULL with a default is
-- forward-compatible; we just don't have the data yet.

ALTER TABLE transactions ALTER COLUMN sender_pubkey DROP NOT NULL;
ALTER TABLE transactions ALTER COLUMN nonce         DROP NOT NULL;
ALTER TABLE transactions ALTER COLUMN fee_paid_nano DROP NOT NULL;
