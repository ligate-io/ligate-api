-- Allow 'bounty_event' and 'contract_event' in the transactions.kind
-- CHECK constraint.
--
-- The v0.4.0 bounty + contract modules added two new tx kinds, and
-- `insert_transaction` writes `kind = 'bounty_event'` / `'contract_event'`
-- (one label per module; the specific lifecycle transition lives in
-- `details.event`). But `transactions_kind_known` (from
-- `20260509000001_indexer_query_tables.sql`) only permitted the original
-- kinds, so every PostBounty / PostContract tx-row INSERT failed the
-- CHECK once the sender NOT NULL gap (`20260529000002`) was cleared.
-- Same downstream effect: the tx is skipped (the resource-row fan-out
-- only runs after the tx row lands), so `/v1/bounties` + `/v1/contracts`
-- never populate even though the on-chain events execute successfully.
ALTER TABLE transactions DROP CONSTRAINT transactions_kind_known;
ALTER TABLE transactions ADD CONSTRAINT transactions_kind_known
    CHECK (kind IN (
        'transfer',
        'register_attestor_set',
        'register_schema',
        'submit_attestation',
        'bounty_event',
        'contract_event',
        'unknown'
    ));
