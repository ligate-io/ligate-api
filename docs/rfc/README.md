# RFCs

Service-level design decisions that cross repo boundaries. RFCs here pin
contracts that the SDK ([`ligate-js`](https://github.com/ligate-io/ligate-js)),
the explorer ([`ligate-explorer`](https://github.com/ligate-io/ligate-explorer)),
and the indexer (in this repo) all build against. Pinning the contracts
here, not in code, means each repo can implement against the spec
without coordinating mid-flight.

These are NOT protocol changes — those are
[chain LIPs](https://github.com/ligate-io/ligate-chain/tree/main/docs/protocol/lips).
RFCs in this repo cover the API surface, not the on-chain runtime.

## Status

| RFC | Title | Status |
|---|---|---|
| [0001](./0001-pagination.md) | Cursor pagination shape for list endpoints | Draft |
| [0002](./0002-response-shapes.md) | Response shape contract for tx, schema, address, info | Draft |
| [0003](./0003-drip-status.md) | Per-address drip-status UX | Draft |

## Process

1. Open RFC PR with status `Draft`.
2. Two-day comment window minimum (skipped if cross-repo work is blocked
   on the answer; nothing's blocked pre-devnet).
3. Status flips to `Accepted` on merge.
4. Implementation PRs reference the RFC in their bodies.
5. If implementation surfaces a wrinkle the RFC didn't cover, amend the
   RFC in the same PR (the spec is normative, drift is a bug).
