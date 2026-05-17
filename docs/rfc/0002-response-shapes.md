# RFC 0002 — Response shape contract for tx, schema, address, info

| | |
|---|---|
| Status | Draft |
| Created | 2026-05-09 |
| Tracks | [#4](https://github.com/ligate-io/ligate-api/issues/4) |
| Touches | ligate-api, ligate-js, ligate-explorer |
| Depends on | [RFC 0001](./0001-pagination.md) |

## Abstract

Pin the JSON shapes ligate-api returns for `Block`, `Tx`, `Schema`,
`AttestorSet`, `AddressSummary`, and `Info`. All amounts are decimal
strings, all timestamps are RFC3339 with millisecond precision, all ids
are bech32m strings (canonical form). Errors return
`{"error": "<msg>", "tracking": "<url-or-null>"}` with conventional HTTP
status codes. Single-resource endpoints unwrap the resource directly;
list endpoints wrap in the [RFC 0001](./0001-pagination.md) envelope.

## Motivation

Same as RFC 0001's: three repos consume these shapes, picking once is
cheaper than refactoring three repos. Specifically:

- ligate-js currently types its indexer methods as `<T = unknown>`
  pending this RFC. Once shapes pin, it ships typed responses.
- The explorer's mock-API mode (`USE_MOCK_API=true`) renders against
  whatever shape we agree on here. If the real API ships a different
  shape, the explorer breaks the day mock-mode is flipped off.
- The chain emits raw bytes; the indexer transforms them. We need to
  decide what's normalised at the indexer layer vs. what the client
  parses.

## Specification

### Encoding rules (apply across all shapes)

| Domain | Encoding | Why |
|---|---|---|
| u128 amounts | Decimal string | JSON `number` is f64; loses precision past 2^53 |
| u64 / i64 | JSON number | Fits in f64 for values up to 2^53 |
| Timestamps | RFC3339 with milliseconds (`"2026-05-09T01:23:45.678Z"`) | Human-readable + parser-stable |
| Block height / slot | JSON number (u64 fits) | `"slot": 12345` |
| Tx hash | bech32m `ltx1...` | `"hash": "ltx1..."`; chain accepts `0x...` hex on URL paths for backward compat |
| Block / slot hash | bech32m `lblk1...` | `"hash": "lblk1..."`; chain accepts hex on input |
| State root | bech32m `lsr1...` | 64-byte payload (NOMT storage backend) |
| Batch hash | bech32m `lba1...` | Canonical |
| DA blob hash | bech32m `lbz1...` | Surfaced in operator logs + sequencer receipts |
| Chain hash | bech32m `lsch1...` | Wallet/schema commitment; returned by `/v1/info` and `/v1/rollup/schema` |
| Address | bech32m `lig1...` | Canonical, no hex variants |
| Schema id | bech32m `lsc1...` | Canonical |
| Attestor set id | bech32m `las1...` | Canonical |
| Attestation id | bech32m `lat1...` | Canonical (v0.2.0; SHA-256 of `schema_id` ‖ `payload_hash`) |
| Payload hash | bech32m `lph1...` | Canonical |
| Pubkey | bech32m `lpk1...` | Canonical |
| Token id | bech32m `token_1...` | Canonical |
| Optional fields | Always present, `null` if absent | No `undefined` vs `missing` ambiguity |

Rationale on amounts: a partner's `1_000_000_000_000_000_000_000n` (1
billion LGT in nano-LGT) breaks `JSON.parse` silently in JS. Decimal
strings round-trip via `BigInt(str)` cleanly. Cost: clients pay one
`BigInt()` parse per field. Worth it.

Rationale on canonical ids: every id has exactly one bech32m form
(lowercase, fixed HRP, fixed length). Partners doing string
comparisons can `===` two ids without a normalisation step.

### Resource shapes

#### `Info` — `GET /v1/info`

```jsonc
{
  "chain_id": "ligate-devnet-1",          // string, from chain config
  "chain_hash": "lsch1amq80arndh6...",    // bech32m with HRP `lsch`
  "version": "0.1.0-devnet",              // node binary semver
  "indexer_height": 12345,                // last slot the indexer has fully ingested
  "head_height": 12345,                   // last slot the chain has finalised (could be > indexer_height during catch-up)
  "head_lag_slots": 0                     // head_height - indexer_height
}
```

The indexer/head split is what the explorer needs to render a "catching
up" badge. Partners who only need chain identity can ignore the
indexer fields.

#### `Block` — `GET /v1/blocks/{height}`

```jsonc
{
  "height": 12345,
  "hash": "lblk1...",                      // 32-byte block hash, bech32m HRP `lblk`
  "parent_hash": "lblk1...",
  "timestamp": "2026-05-09T01:23:45.678Z",
  "tx_count": 7,
  "proposer": "lig1...",                   // sequencer address that proposed the block
  "size_bytes": 1024
}
```

`tx_count` is denormalised at indexer-write time so list views don't
need a join. Detailed tx list lives at `/v1/blocks/{height}/txs`
(future endpoint, not in v0).

#### `Tx` — `GET /v1/txs/{hash}`

```jsonc
{
  "hash": "ltx1...",
  "block_height": 12345,
  "block_hash": "lblk1...",
  "block_timestamp": "2026-05-09T01:23:45.678Z",
  "position": 0,                           // index within the block
  "sender": "lig1...",                     // address derived from pubkey[..28]
  "sender_pubkey": "lpk1...",
  "nonce": 42,
  "fee_paid_nano": "1000000",              // u128 string, in nano-LGT
  "kind": "transfer",                      // tagged union: see "Tx kinds" below
  "details": {
    // shape depends on `kind`
  },
  "outcome": "committed",                  // "committed" | "reverted"
  "revert_reason": null                    // string when outcome=reverted, else null
}
```

##### Tx kinds (`details` field)

| `kind` | `details` shape |
|---|---|
| `"transfer"` | `{ "to": "lig1...", "amount_nano": "1000000000", "token_id": "token_1..." }` |
| `"register_attestor_set"` | `{ "attestor_set_id": "las1...", "members": ["lpk1...", ...], "threshold": 3 }` |
| `"register_schema"` | `{ "schema_id": "lsc1...", "name": "themisra.proof-of-prompt", "version": 1, "attestor_set_id": "las1...", "fee_routing_bps": 0, "fee_routing_addr": null, "payload_shape_hash": "0xhex..." }` |
| `"submit_attestation"` | `{ "schema_id": "lsc1...", "payload_hash": "lph1...", "signature_count": 5 }` |
| `"unknown"` | `{ "raw_call_disc": [9, 99] }` (forward-compat for new chain calls) |

`unknown` is the catch-all for runtime calls the indexer doesn't yet
parse — the indexer pins module + variant discriminants, and unknown
combinations get this shape rather than crashing the ingest. Lets us
ship indexer updates async with chain upgrades.

#### `Schema` — `GET /v1/schemas/{id}`

```jsonc
{
  "id": "lsc1...",
  "name": "themisra.proof-of-prompt",
  "version": 1,
  "owner": "lig1...",
  "attestor_set_id": "las1...",
  "fee_routing_bps": 0,
  "fee_routing_addr": null,
  "payload_shape_hash": "0xhex...",
  "registered_at": {
    "block_height": 12345,
    "tx_hash": "ltx1...",
    "timestamp": "2026-05-09T01:23:45.678Z"
  },
  "attestation_count": 1234              // running total, denormalised
}
```

#### `AttestorSet` — `GET /v1/attestor-sets/{id}`

```jsonc
{
  "id": "las1...",
  "members": ["lpk1...", "lpk1...", ...],
  "threshold": 3,
  "registered_at": {
    "block_height": 12345,
    "tx_hash": "ltx1...",
    "timestamp": "2026-05-09T01:23:45.678Z"
  },
  "schema_count": 5                      // schemas bound to this set; denormalised
}
```

#### `AddressSummary` — `GET /v1/addresses/{addr}`

```jsonc
{
  "address": "lig1...",
  "balances": [
    { "token_id": "token_1...", "amount_nano": "5000000000" },
    { "token_id": "token_1xyz...", "amount_nano": "1234" }
  ],
  "tx_count": 42,                        // total txs sent + received
  "first_seen": {
    "block_height": 12345,
    "timestamp": "2026-05-09T01:23:45.678Z"
  },
  "last_seen": {
    "block_height": 12999,
    "timestamp": "2026-05-09T03:00:00.000Z"
  },
  "schemas_owned_count": 1,              // schemas where owner == this address
  "attestor_member_count": 0             // attestor sets where this address's pubkey is a member
}
```

### List wrappers

Every list endpoint returns the [RFC 0001](./0001-pagination.md)
envelope wrapping the appropriate resource:

```jsonc
{
  "data": [<Block>, <Block>, ...],
  "pagination": { "next": "<cursor>", "limit": 20 }
}
```

### Error shape

```jsonc
{
  "error": "schema not found",
  "tracking": "https://github.com/ligate-io/ligate-api/issues/1"  // optional
}
```

| Condition | HTTP | `error` |
|---|---|---|
| Resource not found | 404 | `"<resource> not found"` |
| Bad request (malformed id, etc.) | 400 | `"<reason>"` |
| Unimplemented endpoint | 501 | `"<endpoint> is not implemented yet"` (with tracking) |
| Internal error | 500 | `"internal error"` (don't leak internals) |
| Rate-limited (drip only) | 429 | `"address rate-limited; retry in N seconds"` (with `retry_after_secs`) |

Existing handlers in `crates/api/src/handlers.rs` already conform on
the 501 path (`tracking: <issue-url>`). The new shape just standardises
across all error categories.

### Versioning

This RFC is the v1 contract. Breaking changes (renaming a field,
changing an encoding) require a new endpoint family (`/v2/...`),
NOT silent shape evolution under `/v1`. Additive changes (new fields
on existing resources) are allowed in v1; clients MUST tolerate
unknown fields.

## Alternatives considered

### u128 amounts as JSON numbers

Browser-side `JSON.parse('{"amount": 1000000000000000000000}')` returns
`1e+21` — loses precision past 2^53. Decimal strings round-trip
cleanly via `BigInt`. Cost: one parse step per field, negligible.

### Hex everywhere

Hex strings for ids would be shorter and uniform. But bech32m is what
the chain emits in its own logs and the cli outputs, so partners
working across the stack see the same format. Hex stays available
inside ligate-js via `tokenIdToHex` / `attestationIdToHex` helpers.

### Embed full block data inside Tx response

Reduces round-trips (one `getTx` call gets you `Tx` + the full `Block`).
Costs payload size (most clients don't need block details). Defer:
`include=block` query param if a partner asks; not v1.

### Wrap single-resource endpoints in `{data: ...}` too

Symmetric with list endpoints, but the list envelope exists *because*
it carries pagination metadata; single resources have nothing to wrap.
Forcing the wrapper just to look uniform is ceremony. Skip.

## Implementation notes

- The indexer's Postgres schema (issue #6) writes the bech32m strings
  directly, not raw bytes. Saves an encode step per query.
- u128 amounts stored as Postgres `numeric` (arbitrary precision); cast
  to string at serialise time.
- Timestamps stored as `timestamptz`; serialised as RFC3339 millis via
  `chrono::DateTime::to_rfc3339_opts(SecondsFormat::Millis, true)`.
- ligate-js's response types pin from this RFC, version-locked: when
  this RFC bumps to v2, the SDK ships `@ligate-labs/sdk@0.1.0`.
