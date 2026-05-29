# Changelog

All notable changes to `ligate-api`. Pre-launch; everything sits under `[Unreleased]` until the first tagged release alongside `ligate-devnet-1`.

Format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/). Issue and PR numbers reference [`ligate-io/ligate-api`](https://github.com/ligate-io/ligate-api).

## [Unreleased]

### Changed (BREAKING — wire format + env vars)

- **Token symbol renamed `$LGT` → `AVOW` everywhere** (tracks `ligate-chain#457`, lands alongside chain v0.3.0). The dollar-prefix prose ticker drops; the new symbol is plain `AVOW`, 4 chars, no `$`. Substitutions cover three surfaces:
  - **Env vars** (operator coordination required at Railway):
    - `LGT_TOKEN_ID` → `AVOW_TOKEN_ID`
    - `LGT_TREASURY_ADDR` → `AVOW_TREASURY_ADDR`
  - **Wire response fields**:
    - `DripResponse.drip_amount_lgt` → `drip_amount_avow` (f64 human-display field on `POST /v1/drip`)
    - `DripBotResponse.amount_lgt` → `amount_avow` (f64 human-display field on `POST /v1/drip-bot`)
    - `TopHoldersResponse.holders[].balance_lgt` → `balance_avow` (f64 human-display field on `GET /v1/stats/top-holders`)
  - **Rust identifiers** (internal, but called out for diff readers): `Config.lgt_token_id_hex` → `avow_token_id_hex`, `Config.lgt_treasury_addr` → `avow_treasury_addr`, `Signer.lgt_token_id` → `avow_token_id`, `Signer.lgt_token_id_bech32()` → `avow_token_id_bech32()`, `handlers::nano_to_lgt` → `nano_to_avow`, `bot_drip::nano_to_lgt` → `nano_to_avow`.
- **Chain dep pin: `branch = "main"` → `tag = "v0.3.0"`** for `attestation`, `ligate-client`, `ligate-stf`, `ligate-rollup`. Pins reproducibly to the known-compatible chain release; bump deliberately rather than drifting on chain main. Matching SDK pin under `[patch."https://github.com/Sovereign-Labs/sovereign-sdk.git"]` left alone since chain v0.3.0 was built against the same fork rev (`eab3f9d0`).
- **Local-dev path**: chain v0.3.0 renamed `devnet/` → `localnet/`. README quickstart updated to reference `~/Desktop/ligate-chain/localnet/` and `localnet/genesis/bank.json`. The actual local chain_id stays `ligate-localnet`; the AVOW token id literal stays `token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7`.
- **Past CHANGELOG entries retroactively rewritten**: `LGT` → `AVOW`, `nano-LGT` → `nano-AVOW` across the [0.2.1] and [0.1.0-devnet] dated sections. Amounts and PR numbers are unchanged; only the ticker text. The historical PRs themselves shipped under the `$LGT` name; the rewrite is for documentation-consistency, not to misattribute past work.

### Migration / coordination

Hold this release until chain v0.3.0 is deployed to whatever public devnet replaces `ligate-devnet-1` (likely `ligate-devnet-2`). Operator sequence:

1. Chain v0.3.0 deploys, fresh genesis, `chain_hash` differs from devnet-1.
2. Update Railway env vars: rename `LGT_TOKEN_ID` → `AVOW_TOKEN_ID`, `LGT_TREASURY_ADDR` → `AVOW_TREASURY_ADDR`. Update `CHAIN_HASH` + `CHAIN_RPC` to point at the new chain.
3. Redeploy api against the new chain.

Until the chain side flips, this api commit is incompatible with the still-LGT devnet-1.

### Added

- `GET /v1/attestor-sets/by-member/{pubkey}` — paginated list of attestor sets whose `members` JSONB array contains the given bech32m `lpk1...` pubkey. Same `Page<AttestorSetResponse>` envelope and `(registered_at_slot, id)` cursor as `/v1/attestor-sets`, so dashboard clients reuse the existing pagination plumbing. Uses the GIN index on `attestor_sets.members` (already present from the original indexer migration) via the JSONB `@>` operator, so the WHERE is an index seek not a full table scan. `schema_count` is read from the denormalised column the indexer already maintains, not recomputed via a per-row LEFT JOIN. Path-param `pubkey` is bech32m-validated (HRP `lpk` + 32-byte payload); typos return 400 `invalid_pubkey` instead of an opaque empty 200. Empty memberships return 200 with `data: []` (absence is a valid answer, not a missing resource). Powers the themisra-dashboard Settings panel's "you're a member of N sets" view (closes audit gap #4 from `ligate-io/themisra-dashboard#34`).
- Three activity-leaderboard endpoints under `/v1/stats/`:
  - `GET /top-attesters?n=10` — addresses ranked by attestation submissions (`attestations.submitter`). All-time count.
  - `GET /top-schema-owners?n=10` — addresses ranked by schemas registered (`schemas.owner`). All-time count.
  - `GET /top-attestor-sets?n=10` — attestor sets (`las1...`) ranked by `attestor_sets.schema_count` (denormalised counter maintained by ingest, so the query is a single index scan).
  All three share a `LeaderboardResponse { window: "all-time", rows: [{ rank, address, count }] }` shape so Grafana table panels can use one template per leaderboard. `n` defaults to 10, capped at 100. 30s in-process cache + `Cache-Control: public, max-age=30` matching the rest of `/v1/stats/*`. Powers the chain Grafana dashboard's forthcoming "Activity leaders" row (filed as a follow-up issue) and a future explorer leaderboard page.
- `slots.da_block_height` column (migration `20260518000002_slots_da_block_height.sql`) + matching `da_block_height: Option<u64>` field on `BlockResponse`. The indexer's `extract_slot_first_batch_facts` (renamed from `extract_slot_proposer` to reflect that it now pulls two related fields from the same first-batch fetch) reads `receipt.da_block_height` from chain v0.2.3+ batch JSON and writes the BIGINT through the existing slot upsert. COALESCE-preserve semantics mirror the `proposer` column: a re-poll that can't reach batches doesn't blank a known value. `null` on slots ingested before chain v0.2.3 (no backfill yet) and on slots whose first-batch fetch fails. Powers the explorer's "View on Celenium" deep-link per `ligate-io/ligate-chain#355` (`https://mocha.celenium.io/block/{da_block_height}` — singular `/block/`; `/blocks` is the list page).
- `GET /v1/stats/drips-daily?days=N` endpoint. Returns daily faucet-drip counts broken down by source: `web` (chain-side count of `bank.transfer` txs from the faucet sender, read from the indexer's `transactions` table) and `bot` (api-side count from `bot_drips`). Powers the cost dashboard's drips-per-day panel without Grafana needing to aggregate two heterogeneous sources client-side. Same `DailyPoint`-style shape as `/v1/stats/attestations-daily`. 30s cached, capped at 90 days of history.
- `POST /v1/drip-bot` endpoint for the Discord faucet bot. Header-gated via `X-Bot-Secret`; uses the same hot-key signer as `POST /v1/drip` so there's a single nonce stream and no inter-endpoint coordination. Tier-aware amounts validated server-side (100 / 250 / 500 / 1000 AVOW for newcomer / regular / veteran / elder, by Discord server tenure). 5-day cooldown, applied independently to (a) per-address and (b) per-Discord-user counters; both must clear. Endpoint disabled (returns 503) if `FAUCET_BOT_SECRET` is unset, so safe to merge before the bot is deployed.
- `bot_drips` Postgres table for durable cooldown state (`migrations/20260518000001_bot_drips.sql`). Two B-tree indexes on `(address, dripped_at DESC)` and `(discord_user_id, dripped_at DESC)` so each cooldown check is a single index seek. Cooldowns persisted to Postgres (not in-memory like the web faucet's `RateLimiter`) because Railway restarts during a 5-day window would otherwise lose multi-day cooldowns.
- New `Config` fields: `FAUCET_BOT_SECRET` (None = endpoint disabled), `BOT_DRIP_RATE_LIMIT_SECS` (default 432000s = 5d), `BOT_DRIP_AMOUNT_{NEWCOMER,REGULAR,VETERAN,ELDER}` (defaults match the proposal). All env-tunable for later curve adjustments without code changes.
- Stacking model: a user can hit both faucets on the same address: 100 AVOW/24h from `/v1/drip` (anon) AND up to 1000 AVOW/5d from `/v1/drip-bot` (Discord-tier). The two counters are independent so each clears on its own schedule.

### Fixed

- **Transactions now expose the real signer pubkey + nonce + sender** (`#550`, part 2). The indexer decodes the borsh-encoded signed-tx body the chain persists once `runner.save_tx_bodies = true` (chain `#551`) and surfaces via `LedgerTx.body.data`, populating the `transactions.sender_pubkey` (`lpk1...`), `nonce`, and `sender` (`lig1...`) columns that previously came back `null`/`0` on the explorer tx-detail page. New module `crates/indexer/src/decode.rs`: a small, fully-validated byte reader rather than a borsh decode of `sov_modules_api::Transaction`, so the indexer stays decoupled from the chain workspace + pinned SDK (no build-time coupling). `sender_pubkey` is read at a fixed offset (so always exact); `sender` is `bech32m("lig", pub_key[0..28])`, matching the chain's no-hash Ed25519 credential rule and cross-checked byte-for-byte against the on-chain `Bank/TokenTransferred` `from`; `nonce` is recovered by a tail-anchored parse keyed on the numeric `CHAIN_ID`, falling back to `null` (never a guessed value) if the anchor or tag bytes do not validate. A 5-byte `AuthenticatorInput::Standard(RawTx)` wrapper on the stored body is detected and stripped (a bare transaction is also handled). The decoded signer is preferred over the event-derived sender (it is the actual fee payer / nonce owner and is the only sender source for bounty/contract kinds, whose thin events carry none); body-less txs ingested before the chain restart stay `null` and keep the event-derived sender fallback. New `IndexerConfig.numeric_chain_id` (threaded from the existing `CHAIN_ID` env) anchors the nonce parse. Snapshot-tested against a real devnet-3 tx body. No migration: the columns were already nullable.

## [0.2.1] - 2026-05-17

Cut alongside `ligate-chain` v0.2.0, `ligate-cli` v0.2.0, `ligate-js` v0.2.0, and `ligate-explorer` for the cross-repo AttestationId wire-format change. Version jumps from `v0.1.0-devnet` to `v0.2.1` to align with the chain's clean-semver convention (chain#374) and reflect that this is the next breaking-compatible release on the api side.

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
- **`/v1/stats/totals`** — single object with all chain-level counts (blocks, txs, addresses, schemas, attestor sets, attestations, total AVOW supply, treasury balance, treasury address). Treasury fields added in (#42).
- **`/v1/stats/finality`** — DA finalization p50 / p95 / p99 percentiles. Observed sampling over last 1h of `slots.finalized_at - slots.timestamp`; falls back to hardcoded estimate when sample count < 20. `source` flips from "estimated" to "observed" once enough flips are logged. (#44)
- **`/v1/stats/next-block-eta`** — live block-cadence prediction. Mean + p95 interval over last 100 slots, `expected_next_at`, `seconds_until_expected` (negative when overdue), `indexer_lag_secs` (true `(chain_head - last_indexed_height) × mean` after #46). (#43, #46)
- **`/v1/stats/active-addresses`, `/v1/stats/new-wallets-daily`, `/v1/stats/tx-rate-daily`, `/v1/stats/top-holders`** — growth + distribution metrics powering both the explorer key-numbers row and the [investor Grafana dashboard](https://ligate.grafana.net/d/ligate-investor).
- **`/v1/stats/attestations-daily`** — daily count of attestations submitted, bucketed by UTC day, default 30d window. Same `{date, count}` shape as `/v1/stats/new-wallets-daily`. Powers the explorer's "DAILY ATTESTATIONS" heatmap. (#53)
- **`/v1/drip`** + **`/v1/drip/status`** — faucet with per-address per-window rate limit, drip budget sanity check on startup, per-address eligibility peek for the explorer faucet UI.

### Storage / schema

- `transactions.protocol_fee_nano` column (migration 0005) — distinct from `fee_paid_nano` (gas). Flat per-call-type module fee routed to treasury / builder share via the schema's `fee_routing_bps`. Devnet-1 values: register_attestor_set = 0.05 AVOW, register_schema = 0.10 AVOW, submit_attestation = 0.0001 AVOW, transfer = 0. (#43)
- `slots.proposer`, `slots.finality_status`, `slots.finalized_at` columns (migration 0006). Plus `slots.prev_hash` backfill via correlated subquery for historical rows. (#44)
- `transactions.fee_paid_nano` backfilled to `0` (migration 0007). Future inserts write 0 explicitly rather than NULL — gas pricing on devnet bills 0 (`gas_used = [0, 0]` even though `gas_price = [7, 7]`), so "0 AVOW (real)" is more honest than "null (unknown)". (#49)

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
- Two queries.rs docstrings claimed module-default fee values (`10/100/0.001 AVOW`); corrected to actual devnet-1 genesis overrides (`0.05/0.10/0.0001 AVOW`). The `fee_paid_nano` docstring also corrected from "gas_price = 0" to "gas_used = 0" — chain meters but doesn't bill in v0. (#47)

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
- Startup drip-budget sanity check (mirror of faucet#7): the api queries the drip signer's own AVOW balance on boot via `Submitter::get_balance_for_holder`, divides by `DRIP_AMOUNT`, and refuses to start if the budget covers fewer than `DRIP_MIN_BUDGET` drips (default 100; set to `0` to skip). Catches the typo class "operator set `DRIP_AMOUNT` to whole-AVOW instead of nano-AVOW (1e9× too much) and would drain the hot key in a handful of drips" before drips actually start.
- CI workflow at `.github/workflows/ci.yml`: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo check`, `cargo test`. Single `CI pass` summary job mirrors the chain-repo / cli / faucet pattern. License: dual-licensed `Apache-2.0 OR MIT`.

### Inherited from upstream archived repos

- All `ligate-io/faucet` features through PR #7: real chain-submit pipeline (no double-wrap, HTTP polling on `/v1/ledger/txs/{hash}`, auto-`/v1` URL normalisation), permissive CORS, startup drip-budget sanity check, env-var-driven config, in-memory per-address rate limiter, structured JSON logs.
- All `ligate-io/ligate-explorer` Rust-side features through PR #1: `NodeClient` REST shim against the chain, sqlx-based Postgres pool, slot backfill + tail loop, chain-info bootstrap migration.
