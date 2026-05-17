# Changelog

All notable changes to `ligate-api`. Pre-launch; everything sits under `[Unreleased]` until the first tagged release alongside `ligate-devnet-1`.

Format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/). Issue and PR numbers reference [`ligate-io/ligate-api`](https://github.com/ligate-io/ligate-api).

## [Unreleased]

## [0.2.1] - 2026-05-17

Cut alongside `ligate-chain` v0.2.0, `ligate-cli` v0.2.0, `ligate-js` v0.2.0, and `ligate-explorer` for the cross-repo AttestationId wire-format change. Version jumps from `0.1.0-devnet` to `0.2.1` to align with the chain's clean-semver convention (chain#374) and reflect that this is the next breaking-compatible release on the api side.

### Changed (BREAKING — wire format)

- **`AttestationId` collapsed to `lat1...`** (chain#381 / api#56). The compound `<schema_id>:<payload_hash>` (`lsc1...:lph1...`) form is replaced by a single 32-byte bech32m hash with the `lat` HRP, derived as `SHA-256(schema_id_bytes ‖ payload_hash_bytes)`. Mirrors the chain reference at `ligate-chain/crates/modules/attestation/src/lib.rs::AttestationId::from_pair` and is snapshot-tested in `crates/indexer/src/attestation_id.rs` against the chain's `borsh_snapshot.rs` vector.
- **`GET /v1/attestations/{id}`** now accepts a single bech32m `lat1...` path segment instead of the colon-separated compound form. Returns 400 on any other prefix.
- **`AttestationResponse.id`** is now the `lat1...` form. Constituent `schema_id` + `payload_hash` remain as separate fields on the response body for callers that need them.
- **`/v1/search`** drops the composite-id branch (`lsc1...:lph1...`) and adds a `lat1...` branch. The `lph1...` payload-hash branch now returns the canonical `lat1...` id of the first matching attestation instead of the `(schema_id, payload_hash)` pair.
- **`SearchResponse::Attestation`** payload shape: `{ "kind": "attestation", "id": "lat1..." }` (was `{ "kind": "attestation", "schema_id": "lsc1...", "payload_hash": "lph1..." }`). Clients that need the constituents fetch `/v1/attestations/{id}` for the full body.

### Storage / schema

- New `attestations.id TEXT NOT NULL` column with `UNIQUE INDEX attestations_id_unique` (migration `20260517000001_attestation_id_lat.sql`). The indexer writes the `lat1...` form derived at parse time; UPSERTs target `ON CONFLICT (id)` so re-submissions of the same logical attestation fold into the existing row instead of inserting duplicates. Migration TRUNCATEs the table on the assumption that the operator ran a devnet re-genesis before applying (no in-SQL backfill path exists; SHA-256 + bech32m aren't available as Postgres functions). The indexer re-populates from chain history on the next ingest pass.

### Added

- `crates/indexer/src/attestation_id.rs`: `compute_attestation_id(schema_id, payload_hash) -> Result<String, AttestationIdError>` helper. Pure function, snapshot-tested against the chain's reference vector (`schema_id = [0x11; 32], payload_hash = [0x33; 32]` SHA-256s to `b0dcb09af5496e779e60b21109a718475091191efc7a8638b01d51c622fc9128`).
- `bech32 = "0.11"` + `sha2 = "0.11"` workspace deps (versions match `ligate-chain`'s workspace).
- `IndexerSubmitAttestation.id` (parser-side), `AttestationRow.id` (query-side), and the corresponding column in every `SELECT FROM attestations` statement.

### Removed

- `queries::attestation_by_pair(schema_id, payload_hash)` (use `attestation_by_id(lat1...)`).
- `queries::attestation_pair_exists(schema_id, payload_hash)` (use `attestation_id_exists(lat1...)`).
- `queries::attestation_by_payload_hash` return shape changed to `Option<String>` (the `lat1...` id) instead of `Option<(String, String)>`; the helper was renamed to `attestation_id_by_payload_hash` to reflect that.

### Migration / coordination

This release moves in lockstep with chain v0.2.0. Operators applying the new migration on a populated devnet DB will lose all `attestations` rows (TRUNCATE); the operator runbook for devnet re-genesis is the source of truth for sequencing. The chain crates pinned in `Cargo.toml` (`attestation`, `ligate-client`, `ligate-stf`, `ligate-rollup`) remain on `branch = "main"`; once chain v0.2.0 merges to `main` the api picks it up on next build.

## [0.1.0-devnet] - 2026-05-16

First tagged release, cut alongside `ligate-chain` `v0.1.1-devnet`, `ligate-cli` `v0.1.2-devnet`, and `ligate-js` `v0.1.1-devnet` for the `ligate-devnet-1` public devnet launch.

The api hosts both the drip (faucet) endpoints and the indexer query surface that backs [explorer.ligate.io](https://explorer.ligate.io). Deployed to Railway (api + Postgres), proxied through Cloudflare for WAF + rate limit + HSTS + HTTPS enforcement.

### Added

- **`/v1/info`** — chain identity + indexer head + chain head + lag. Sources `head_height` from a real chain RPC call (parallel `tokio::join!`) rather than aliasing `indexer_height`, so `head_lag_slots` actually means "how far behind the indexer is." (#46)
- **`/v1/blocks` / `/v1/blocks/{height}`** — slot list + detail. `BlockResponse` carries `height`, `hash`, `parent_hash` (derived from prev slot's hash since chain doesn't emit it), `state_root`, `timestamp`, `tx_count`, `batch_count`, `proposer` (sequencer's Celestia `da_address` from first batch), `size_bytes`, **`finality_status`** ("pending" or "finalized" mirrored from chain), **`finalized_at`** (observed wall-clock when indexer saw pending→finalized). (#44)
- **`/v1/txs` / `/v1/txs/{hash}`** — tx list + detail. Supports `?kind=` (transfer / register_attestor_set / register_schema / submit_attestation / unknown) and `?block_height=N` filters; both compose with the compound `(slot, position)` cursor pagination. (#43, #50)
- **`/v1/schemas`, `/v1/attestor-sets`, `/v1/attestations`** — list + detail. (#40 fixed the indexer's attestation event-shape mismatch so these populate at all.) `SchemaResponse` carries `threshold: u8` from the bound attestor set via a JOIN at read time, so the explorer can render "M of N" in the schema list without N+1 fetches per row. (#52)
- **`/v1/addresses/{addr}`** — balance + tx counts + first/last seen + schemas-owned + attestor-set memberships.
- **`/v1/addresses/{addr}/txs`** — paginated tx history for one address. Returns txs where the address participated in any role (`sender` for any kind, or `from` / `to` in a transfer's JSONB details). Same envelope + cursor shape as `/v1/txs`; explorer reuses its existing adapter with a different URL. (#52)
- **`/v1/search?q=...`** — single-endpoint resolver across block height / `lblk1...` block hash / `ltx1...` tx hash / `lig1...` address / `lsc1...` schema / `las1...` attestor set / `lph1...` payload hash / `lsc1...:lph1...` composite attestation id. (#50)
- **`/v1/stats/totals`** — single object with all chain-level counts (blocks, txs, addresses, schemas, attestor sets, attestations, total LGT supply, treasury balance, treasury address). Treasury fields added in (#42).
- **`/v1/stats/finality`** — DA finalization p50 / p95 / p99 percentiles. Observed sampling over last 1h of `slots.finalized_at - slots.timestamp`; falls back to hardcoded estimate when sample count < 20. `source` flips from "estimated" to "observed" once enough flips are logged. (#44)
- **`/v1/stats/next-block-eta`** — live block-cadence prediction. Mean + p95 interval over last 100 slots, `expected_next_at`, `seconds_until_expected` (negative when overdue), `indexer_lag_secs` (true `(chain_head - last_indexed_height) × mean` after #46). (#43, #46)
- **`/v1/stats/active-addresses`, `/v1/stats/new-wallets-daily`, `/v1/stats/tx-rate-daily`, `/v1/stats/top-holders`** — growth + distribution metrics powering both the explorer key-numbers row and the [investor Grafana dashboard](https://ligate.grafana.net/d/ligate-investor).
- **`/v1/stats/attestations-daily`** — daily count of attestations submitted, bucketed by UTC day, default 30d window. Same `{date, count}` shape as `/v1/stats/new-wallets-daily`. Powers the explorer's "DAILY ATTESTATIONS" heatmap. (#53)
- **`/v1/drip`** + **`/v1/drip/status`** — faucet with per-address per-window rate limit, drip budget sanity check on startup, per-address eligibility peek for the explorer faucet UI.

### Storage / schema

- `transactions.protocol_fee_nano` column (migration 0005) — distinct from `fee_paid_nano` (gas). Flat per-call-type module fee routed to treasury / builder share via the schema's `fee_routing_bps`. Devnet-1 values: register_attestor_set = 0.05 LGT, register_schema = 0.10 LGT, submit_attestation = 0.0001 LGT, transfer = 0. (#43)
- `slots.proposer`, `slots.finality_status`, `slots.finalized_at` columns (migration 0006). Plus `slots.prev_hash` backfill via correlated subquery for historical rows. (#44)
- `transactions.fee_paid_nano` backfilled to `0` (migration 0007). Future inserts write 0 explicitly rather than NULL — gas pricing on devnet bills 0 (`gas_used = [0, 0]` even though `gas_price = [7, 7]`), so "0 LGT (real)" is more honest than "null (unknown)". (#49)

### Performance

- **`Cache-Control` headers on 11 endpoints** (#49). Per-endpoint TTLs tuned to volatility: 5s for live (`/v1/info`, `/v1/blocks` list, `/v1/txs` list, `/v1/stats/next-block-eta`), 30s for modest (`/v1/attestations` list, address summary, most `/v1/stats/*`), 60s for slow (`/v1/schemas` list, `/v1/attestor-sets` list), **300s for immutable content-addressed resources** (`/v1/blocks/{h}`, `/v1/txs/{h}`, `/v1/attestations/{id}`, `/v1/schemas/{id}`, `/v1/attestor-sets/{id}`). Expected explorer cold-home TTFB drop from ~640ms to ~80ms on warm renders (Vercel edge + Next.js fetch cache both honor downstream).

### Fixed

- `/v1/stats/totals` returns `total_supply_nano` correctly — was hitting `0x<hex>` path; chain only accepts bech32m `token_1...`. (#42)
- `/v1/attestations` no longer 500s on rows where `submitter_pubkey IS NULL` (post-migration 0004 made it nullable; serialization needed update). (#42)
- `/v1/txs?kind=` server-side filter — was a no-op (param parsed but never threaded into SQL). Now properly dispatches. (#43)
- `/v1/stats/next-block-eta.indexer_lag_secs` — was literally `seconds_since_last` renamed, cycling 0 → mean-interval each block. Now reports true `(chain_head - last_indexed_height) × mean_block_interval_secs`. (#46)
- `/v1/info.head_lag_slots` — was hardcoded 0 because `head_height = indexer_height` aliasing. Now reflects real chain head from parallel `latest_slot()` call. (#46)
- `/v1/search?q=lsc1...` and `?q=las1...` 500'd because `SELECT 1` returns int4 but sqlx expected int8. Rewrote both as `SELECT EXISTS(...)` returning a clean bool. (#50)
- `/v1/search?q=lsc1...:lph1...` composite attestation id — previously returned `not_found` (no branch handled it). Added composite-id branch with `attestation_pair_exists` query. (#50)
- Indexer was silently dropping attestation txs because the chain emits `AttestationModule/AttestorSetRegistered` with PascalCase event names + raw bech32m strings, not the `Attestation/` snake_case shape the parser expected. Fixed event matching to chain reality. (#40)
- Two queries.rs docstrings claimed module-default fee values (`10/100/0.001 LGT`); corrected to actual devnet-1 genesis overrides (`0.05/0.10/0.0001 LGT`). The `fee_paid_nano` docstring also corrected from "gas_price = 0" to "gas_used = 0" — chain meters but doesn't bill in v0. (#47)

### Followups (tracked, deferred to post-launch or post-mainnet)

- `/v1/schemas?attestor_set_id=X` filter (api#48 Tier 1.2) — devnet has 1 schema, no scale pressure yet
- `/v1/dashboard` aggregator (api#48 Tier 3.3) — most of the win already captured by Cache-Control
- WebSocket / SSE on `/v1/blocks/stream` (api#48 Tier 3.4) — post-mainnet
- Indexer subscribes to chain `BlobExecutionStatus` for true finalization timestamp instead of observed (api#45)
- Defense-in-depth middleware: tower-governor + body cap + request timeout (api#32)
- Per-IP rate limit on /v1/drip in api code as defense-in-depth alongside the Cloudflare edge rate limit that already shipped (api#31)
- Faucet anti-abuse: Discord-account-age check (api#2)

### Added (initial scaffold)

- Initial scaffold. Cargo workspace with four crates:
  - `crates/drip/` — faucet primitives (`Signer`, `RateLimiter`, errors). Ported from the (now-archived) `ligate-io/faucet` repo with no logic changes; just wrapped as a library so the api crate composes it. Carries forward all the wire-format gotchas the faucet repo discovered: no double-wrap on submit, HTTP polling on `/v1/ledger/txs/{hash}` for inclusion confirmation, idempotent `/v1` URL append.
  - `crates/indexer/` — chain → Postgres ingest task. Ported from `ligate-io/ligate-explorer/crates/indexer/` (now Next.js-only). Currently indexes slots + chain-identity bootstrap; transactions / schemas / attestations come in subsequent PRs.
  - `crates/types/` — shared serde types mirroring the chain REST surface. Ported from `ligate-io/ligate-explorer/crates/types/`.
  - `crates/api/` — binary; axum router that mounts `/v1/drip*` (fully wired against the drip crate) plus stub `/v1/blocks*`, `/v1/txs*`, `/v1/addresses/*`, `/v1/schemas*`, `/v1/info` endpoints (returning 501 until the indexer's Postgres schema solidifies and the query layer fleshes out).
- Multi-stage `Dockerfile` for `linux/amd64` + `linux/arm64`. Two-stage build (Rust toolchain → debian-slim runtime) producing a ~50 MB image with the `ligate-api` binary. Same risc0-skip env vars chain repo's CI uses (`SKIP_GUEST_BUILD=1`, `RISC0_SKIP_BUILD_KERNELS=1`, `CONSTANTS_MANIFEST_PATH`).
- `railway.toml` deploy config: Dockerfile builder, on_failure restart policy, `/health` healthcheck. Postgres plugin auto-wires `DATABASE_URL`; chain-identity vars (`CHAIN_RPC`, `CHAIN_ID`, `CHAIN_HASH`, `LGT_TOKEN_ID`) and `DRIP_SIGNER_KEY` set per-environment in the Railway UI.
- CORS `permissive()` on every public endpoint (mirror of faucet#7) so partner web apps (`mneme.ligate.io`, Themisra demo pages, `explorer.ligate.io` itself) can hit the API from arbitrary origins without preflight blocks. Tighten the origin allow-list at testnet+.
- Startup drip-budget sanity check (mirror of faucet#7): the api queries the drip signer's own LGT balance on boot via `Submitter::get_balance_for_holder`, divides by `DRIP_AMOUNT`, and refuses to start if the budget covers fewer than `DRIP_MIN_BUDGET` drips (default 100; set to `0` to skip). Catches the typo class "operator set `DRIP_AMOUNT` to whole-LGT instead of nano-LGT (1e9× too much) and would drain the hot key in a handful of drips" before drips actually start.
- CI workflow at `.github/workflows/ci.yml`: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo check`, `cargo test`. Single `CI pass` summary job mirrors the chain-repo / cli / faucet pattern. License: dual-licensed `Apache-2.0 OR MIT`.

### Inherited from upstream archived repos

- All `ligate-io/faucet` features through PR #7: real chain-submit pipeline (no double-wrap, HTTP polling on `/v1/ledger/txs/{hash}`, auto-`/v1` URL normalisation), permissive CORS, startup drip-budget sanity check, env-var-driven config, in-memory per-address rate limiter, structured JSON logs.
- All `ligate-io/ligate-explorer` Rust-side features through PR #1: `NodeClient` REST shim against the chain, sqlx-based Postgres pool, slot backfill + tail loop, chain-info bootstrap migration.
