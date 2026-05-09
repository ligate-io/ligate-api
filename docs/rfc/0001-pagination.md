# RFC 0001 — Cursor pagination shape for list endpoints

| | |
|---|---|
| Status | Draft |
| Created | 2026-05-09 |
| Tracks | [#3](https://github.com/ligate-io/ligate-api/issues/3) |
| Touches | ligate-api, ligate-js, ligate-explorer |

## Abstract

Every `list` endpoint (`/v1/blocks`, `/v1/txs`, `/v1/schemas`, etc) ships
the same cursor pagination shape. Cursor is a base64-encoded opaque
token whose internal representation is the endpoint's natural sort key.
Default page size is 20, max is 100. Direction is implicit (descending
by sort key); explicit forward/backward semantics deferred to a future
RFC if/when partner traffic justifies the complexity.

## Motivation

We have eight list endpoints stubbed in `crates/api/src/handlers.rs`,
each returning 501 with a `PaginationParams { limit, before }` struct
that's `#[allow(dead_code)]` because nothing consumes it yet. Before we
write the queries, we need agreement on:

- What `before` is (block height? Postgres bigint id? base64 token?
  ISO timestamp?)
- Whether the response envelope carries pagination metadata (next
  cursor, total count, has-more) or omits it
- What "the next page" means when records are inserted between calls
  (stable cursor vs unstable)
- Default + max page size

Three repos build against the answer (this repo's handlers, ligate-js's
`LigateClient.listBlocks/listTxs/...`, the explorer's React data
hooks). Picking once now is cheaper than refactoring three repos when
we discover an inconsistency.

## Specification

### Request shape

Every list endpoint accepts the same query params:

```
GET /v1/<resource>?limit=<u32>&before=<opaque-cursor>
```

| Param | Type | Required | Default | Max | Notes |
|---|---|---|---|---|---|
| `limit` | u32 | No | 20 | 100 | Server clamps to max silently; no error |
| `before` | string | No | (latest) | — | Opaque cursor; only valid if it came from a prior response |

Any other query params on a list endpoint are ignored (forward-compat
for endpoint-specific filters added later).

### Response envelope

```jsonc
{
  "data": [<resource>, <resource>, ...],
  "pagination": {
    "next": "eyJzbG90IjoxMjM0NX0",  // base64-encoded cursor; null if at end
    "limit": 20                      // echo the resolved limit (post-clamp)
  }
}
```

The envelope is the same on every list endpoint. Empty pages return
`{"data": [], "pagination": {"next": null, "limit": 20}}`, not 404.

`total` (count of all matching records) is intentionally **not** in the
envelope. Computing it requires a second query that doesn't scale
beyond a few million rows; partners that need a count should use a
dedicated `/count` endpoint when one ships.

### Cursor format

The cursor is base64url-encoded JSON. Its internal shape is per-resource;
clients **MUST** treat it as opaque. Examples (decoded for reference):

| Endpoint | Decoded cursor | Sort key |
|---|---|---|
| `/v1/blocks` | `{"slot": 12345}` | block height descending |
| `/v1/txs` | `{"slot": 12345, "idx": 7}` | (slot desc, position-in-block desc) |
| `/v1/schemas` | `{"id": "lsc1..."}` | registration order, scoped by schema_id |
| `/v1/attestor-sets` | `{"id": "las1..."}` | registration order |
| `/v1/addresses/{a}/txs` | `{"slot": 12345, "idx": 7}` | same as `/v1/txs` |

Cursor opacity matters: it lets the server change internal sort keys
later (add a tiebreaker, switch from slot-based to a global tx index)
without breaking clients.

### Sort direction

Always descending by the natural sort key (newest first). This is what
the explorer wants by default for blocks/txs, and matches what partners
actually paginate through (recent activity first).

If/when an endpoint needs ascending order, add it as
`?order=asc|desc` and document the cursor's behavior with the new
direction. Out of scope for this RFC.

### Stability under writes

The cursor pins (sort_key, tiebreaker_id) at the moment the previous
page was generated. New inserts at or before that point appear in the
**next** page, not the **previous** page. Effect: a partner paginating
through `/v1/txs` will see all txs that existed at request 1, plus any
new txs that landed between request 1 and request N — without
duplication.

This is the standard "keyset pagination" guarantee. Trade-off: if you
delete a tx between pages (impossible on this chain — txs are immutable
post-finality), you'd see a phantom skip; not a concern for our domain.

### Error cases

| Condition | HTTP | Body |
|---|---|---|
| `limit` not a u32 | 400 | `{"error": "limit must be a non-negative integer"}` |
| `before` not valid base64url | 400 | `{"error": "invalid cursor"}` |
| `before` decodes but cursor is for a different endpoint | 400 | `{"error": "cursor does not match endpoint"}` |
| `before` references a record that no longer exists | (treated as if absent) | Returns first page |

A "stale cursor" silently returns the latest data rather than 404, on
the principle that the partner's intent was "give me the most recent
records I haven't seen" — and the latest page is the closest answer.

## Alternatives considered

### Offset pagination (`?limit&offset`)

Familiar, but breaks under concurrent inserts (the same record can
appear on two consecutive pages, or skip entirely). Not a fit for an
indexer that's writing live as queries fan out.

### Time-based cursor (`?before=<RFC3339>`)

Readable, but:
- Not all resources are time-ordered (schemas are naturally ordered by
  registration, which is strictly time-monotonic but the cursor would
  duplicate `slot` info already in the data)
- Cosmos chain time has 1s resolution; multiple records per second
  break tiebreaker semantics
- Coupling the cursor to wall-clock time means the server can't change
  its internal sort key later

### Total count in envelope

Discussed above. Cost outweighs benefit for the cardinalities we
expect (>1M rows post-devnet). Add `/v1/<resource>/count` if a partner
asks; don't pay the cost on every list call.

### Forward + backward navigation (`?after=<cursor>` plus `?before=<cursor>`)

Useful for explorer "previous/next page" buttons. Deferred — the
explorer's first cut is "show latest 20, infinite scroll downward"
which only needs `before`. Add `?after=<cursor>` when the UX demands it.

## Implementation notes

- `PaginationParams` in `crates/api/src/handlers.rs` keeps its current
  field names (`limit: Option<u32>`, `before: Option<String>`); just
  removes the `#[allow(dead_code)]` once handlers wire through.
- Default and max limits live in `crates/api/src/config.rs` as
  `DEFAULT_PAGE_LIMIT: u32 = 20` and `MAX_PAGE_LIMIT: u32 = 100`.
- ligate-js's `LigateClient.listBlocks` etc. forward `limit` and
  `before` as-is; partners pass the cursor they got from the previous
  call.
