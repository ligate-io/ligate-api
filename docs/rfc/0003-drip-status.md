# RFC 0003 — Per-address drip-status UX

| | |
|---|---|
| Status | Draft |
| Created | 2026-05-09 |
| Tracks | [#5](https://github.com/ligate-io/ligate-api/issues/5) |
| Touches | ligate-api, ligate-explorer (faucet page), ligate-js |

## Abstract

Split the drip-status surface into operator-facing
(`GET /v1/drip/status`) and partner-facing
(`GET /v1/drip/status/{address}`). The address-scoped endpoint tells
the explorer's faucet page exactly what to render: are you eligible
right now, when's your next window, how much will you receive. The
unscoped endpoint stays for ops dashboards but loses fields the
frontend doesn't need.

## Motivation

The current `GET /v1/drip/status` response shape was written for an
operator dashboard: faucet's signing address, total addresses dripped,
configured rate-limit window. The explorer's faucet page needs
different information:

- "Can this address drip right now?" (green CTA vs grey)
- "When can it drip again?" (countdown)
- "How much will it get?" (display "you'll receive 1 LGT")
- "Is the faucet drained?" (helpful error vs cryptic 502)

Today, the frontend would call `POST /v1/drip` and discover ineligibility
via the 429 response. That works but produces ugly UX (button click,
spinner, error toast) instead of disabled-button-with-countdown.

A per-address status endpoint moves that check up to render time. The
button is correctly enabled or disabled before the user clicks.

## Specification

### `GET /v1/drip/status` — operator-facing (revised)

```jsonc
{
  "drip_amount_nano": "1000000000",       // u128 string per RFC 0002
  "drip_amount_lgt": 1.0,                 // f64 convenience field
  "rate_limit_secs": 86400,
  "min_budget_nano": "100000000000",      // operator early-warning threshold
  "current_budget_nano": "999000000000",  // faucet's $LGT balance, fresh per request
  "faucet_address": "lig1...",
  "addresses_dripped_total": 1234,        // since process start (in-memory counter)
  "drained": false                        // current_budget_nano < drip_amount_nano
}
```

Changes from current shape:
- Adds `current_budget_nano` (refreshed every call from
  `getBalance(faucet_address, lgt_token)`).
- Adds `drained` boolean for at-a-glance ops alerting.
- Renames `addresses_dripped` to `addresses_dripped_total` for clarity.
- u128 amounts as decimal strings per [RFC 0002](./0002-response-shapes.md).

### `GET /v1/drip/status/{address}` — partner-facing (NEW)

```jsonc
{
  "address": "lig1...",
  "eligible": true,                       // computed: not rate-limited AND not drained
  "drip_amount_nano": "1000000000",
  "drip_amount_lgt": 1.0,
  "next_drip_at": null,                   // RFC3339 timestamp; null when eligible=true
  "cooldown_secs_remaining": 0,           // 0 when eligible=true
  "drained": false,                       // explicit "faucet is dry" signal
  "rate_limited": false                   // explicit "you specifically need to wait" signal
}
```

Field semantics:

| Field | Meaning |
|---|---|
| `eligible` | true iff `!drained && !rate_limited` |
| `next_drip_at` | When `rate_limited`, RFC3339 timestamp of when this address can drip again. `null` when eligible. |
| `cooldown_secs_remaining` | Same data as `next_drip_at`, in seconds, for clients that don't want to parse a date. `0` when eligible. |
| `drained` | Faucet's `current_budget_nano < drip_amount_nano`. Same flag the operator endpoint exposes. |
| `rate_limited` | This address tried within `rate_limit_secs`. `eligible` is the boolean to render against; `rate_limited` and `drained` are diagnostic. |

The split between `eligible` (one bool) and the two diagnostic fields
matters for UX copy. The explorer can render:

- `eligible: true` → green "Drip 1 LGT" button
- `rate_limited: true` → "Try again in 23h 47m" with countdown
- `drained: true` → "Faucet refilling — try again later"
- both → "Faucet temporarily drained" (suppress the cooldown noise)

### Address validation

`{address}` is parsed via the same code path the `POST /v1/drip` handler
uses (the bech32m address parser, which lives in the chain's address
crate via the `ligate-client` workspace dep). Invalid bech32m → 400
with `{"error": "invalid address"}`. Valid format but unknown address
on chain → still returns the status (eligibility is purely faucet-state,
not on-chain-state — a never-seen address is eligible iff the faucet
has budget).

### Caching

Operator endpoint: no cache. Hits Postgres + chain RPC every call. Low
volume, real-time matters more than scale.

Partner endpoint: no cache either. The eligibility check is a constant-
time HashMap lookup against the in-memory rate limiter; chain balance
read is cached for 5 seconds (avoid hammering the chain on a high-
traffic faucet page).

The 5-second TTL is enough that a partner who drips, waits a few
seconds, refreshes the page sees the updated state. Not so long that
"faucet drained" sticks around minutes after a refill.

## Alternatives considered

### Single endpoint with optional `?address=...` query param

Keeps URL surface smaller. But the response shapes really are
different — operator wants `addresses_dripped_total`, partner wants
`cooldown_secs_remaining`. Cramming both into one envelope means every
caller pays for the union. Two endpoints is cleaner.

### Server-Sent Events stream of cooldown updates

Live countdown without polling. Overkill for a faucet page that
refreshes once per cycle; partners can just `setInterval(fetch, 1000)`
client-side. SSE adds operational complexity (long-lived connections,
proxies, timeouts) for a UX win that's invisible.

### Embed eligibility directly in `POST /v1/drip` precheck

Could `POST /v1/drip?dry_run=true` return the same shape without
actually dripping. Doable, but conflates two concerns (status query
vs. mutation) and makes the route's contract harder to reason about.
Two endpoints is clearer.

### Drop `cooldown_secs_remaining`, only return `next_drip_at`

Partners would do `(new Date(next_drip_at) - new Date()) / 1000`
themselves. Trivial, but every consumer ends up writing the same
two-line helper. Returning both costs four bytes per response and
saves the helper everywhere.

## Implementation notes

- Existing `RateLimiter::check` returns `RateCheck::Allowed` /
  `RateCheck::Blocked { retry_after }` — the `retry_after` Duration
  is exactly what `cooldown_secs_remaining` and `next_drip_at` need.
- `current_budget_nano` lookup uses `LigateClient::getBalance(faucet_addr, LGT_TOKEN_ID)`
  with a 5-second TTL cache. Cache lives in `AppState` as
  `Arc<RwLock<Option<(Instant, u128)>>>`.
- Both endpoints land in the same handler file
  (`crates/api/src/handlers.rs`); the routes wire up in `main.rs`'s
  router build.
- ligate-js gets a typed `DripStatus` and `AddressDripStatus` interface
  matching these shapes; the existing `client.getDripStatus()` (not yet
  implemented) splits into two methods.
