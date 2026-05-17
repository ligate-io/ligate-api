# ligate-api

[![CI](https://github.com/ligate-io/ligate-api/actions/workflows/ci.yml/badge.svg)](https://github.com/ligate-io/ligate-api/actions/workflows/ci.yml) [![License: Apache-2.0 OR MIT](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license) [![Chain](https://img.shields.io/badge/chain-ligate--devnet--1-A7D28C.svg)](https://github.com/ligate-io/ligate-chain) [![Docs](https://img.shields.io/badge/docs-docs.ligate.io-A7D28C.svg)](https://docs.ligate.io) [![Devnet](https://img.shields.io/badge/status-devnet-A7D28C.svg)](#status)

Unified HTTP API for [Ligate Chain](https://github.com/ligate-io/ligate-chain). Drip (faucet) and indexer queries on a single domain. Deploys to Railway. Backs `explorer.ligate.io`.

## What this is

One Rust service hosting:

- **Drip (faucet)** — `POST /v1/drip`, `GET /v1/drip/status`. Hot-key signs a `bank.transfer` to the requesting address, rate-limited per-address. Replaces the now-archived [`ligate-io/faucet`](https://github.com/ligate-io/faucet) repo.
- **Indexer queries** — `GET /v1/blocks*`, `/v1/txs*`, `/v1/addresses/*`, `/v1/schemas*`, `/v1/info`. Postgres-backed; the indexer task running in the same process keeps the DB current. Replaces the Rust-side of the now-archived [`ligate-io/ligate-explorer`](https://github.com/ligate-io/ligate-explorer) repo.

Two deploy artifacts (one Rust binary, one Postgres) instead of three repos and three deploys. Same domain (`api.ligate.io`) so partners only learn one URL for everything except direct chain RPC.

## Endpoints (v0)

All endpoints are wired and serving on `ligate-devnet-1`. Grouped by surface:

```
# Health / info
GET  /health                                 → 200 {"status":"ok"} (unversioned; orchestrator probe)
GET  /v1/health                              → 200 {"status":"ok"}
GET  /v1/info                                → chain_id, chain_hash, version,
                                                indexer_height, head_height, head_lag_slots

# Blocks
GET  /v1/blocks                              → paginated list of latest blocks
GET  /v1/blocks/{height}                     → block detail

# Transactions
GET  /v1/txs                                 → paginated list of latest txs (?block_height, ?kind)
GET  /v1/txs/{hash}                          → tx detail

# Addresses
GET  /v1/addresses/{addr}                    → balance + recent tx history
GET  /v1/addresses/{addr}/txs                → paginated tx history for one address

# Schemas
GET  /v1/schemas                             → list of registered schemas (?attestor_set_id)
GET  /v1/schemas/{id}                        → schema detail (incl. threshold join)
GET  /v1/schemas/{id}/attestations           → attestations for one schema

# Attestor sets
GET  /v1/attestor-sets                       → list of registered attestor sets
GET  /v1/attestor-sets/{id}                  → attestor-set detail
GET  /v1/attestor-sets/{id}/attestations     → attestations for one attestor set

# Attestations
GET  /v1/attestations                        → paginated list of latest attestations
GET  /v1/attestations/{id}                   → attestation detail (composite id)

# Search
GET  /v1/search                              → unified lookup (lig1, ltx1, lsc1, las1, block height)

# Stats (in-process 30s cache; powers explorer + investor dashboard)
GET  /v1/stats/totals                        → cumulative totals across the chain
GET  /v1/stats/finality                      → finality lag + last-finalized slot
GET  /v1/stats/next-block-eta                → predicted next-block-at (uses true indexer lag)
GET  /v1/stats/active-addresses              → active-addresses windows
GET  /v1/stats/new-wallets-daily             → daily new-wallet counts
GET  /v1/stats/tx-rate-daily                 → daily tx-rate timeseries
GET  /v1/stats/attestations-daily            → daily attestations (powers 30d heatmap)
GET  /v1/stats/top-holders                   → top LGT holders

# Faucet / drip
POST /v1/drip                                → body {address}, returns
                                                {address, tx_hash, amount_nano, drip_amount_lgt}
GET  /v1/drip/status                         → drip_amount_nano, drip_amount_lgt,
                                                rate_limit_secs, addresses_dripped, faucet_address
GET  /v1/drip/status?address={addr}          → {can_drip, next_drip_at} (per-address shape;
                                                untagged enum, same path)
```

Per `crates/api/src/main.rs:217-258`. Pagination shapes, cache headers, and error envelope are documented in `docs/rfcs/` and `docs/queries.md`.

## Architecture

```
┌─────────────────────────┐    ┌─────────────────────────┐
│  ligate-explorer        │    │  Themisra / Mneme       │
│  (Next.js, Vercel)      │    │  (partner web apps)     │
└────────────┬────────────┘    └────────────┬────────────┘
             │                              │
             │     api.ligate.io            │
             ├──────────────────────────────┤
             ▼                              ▼
        ┌──────────────────────────────────┐
        │    ligate-api (Rust, Railway)    │
        │  ┌─────────┐ ┌────────────────┐  │
        │  │  drip   │ │   indexer      │  │
        │  │ /v1/drip│ │ task + queries │  │
        │  └────┬────┘ └────┬───────────┘  │
        │       │           │              │
        └───────┼───────────┼──────────────┘
                │           │
                ▼           ▼
       ┌──────────────┐  ┌────────────────┐
       │  rpc.ligate  │  │   Postgres     │
       │  .io (chain) │  │  (Railway)     │
       │   on GCP     │  │                │
       └──────────────┘  └────────────────┘
```

## Quick start (local dev)

```bash
# 1. Postgres (docker)
docker run --rm -d -p 5432:5432 --name ligate-pg \
    -e POSTGRES_DB=ligate_api \
    -e POSTGRES_PASSWORD=local \
    postgres:16

# 2. Boot ligate-node from chain repo (separate terminal)
cd ~/Desktop/ligate-chain
cargo run --bin ligate-node

# 3. Run ligate-api
cd ~/Desktop/ligate-api
DATABASE_URL=postgres://postgres:local@localhost:5432/ligate_api \
CHAIN_RPC=http://localhost:12346 \
CHAIN_ID=4242 \
CHAIN_HASH=$(curl -s http://localhost:12346/v1/rollup/info | jq -r .chain_hash) \
LGT_TOKEN_ID=token_1nyl0e0yweragfsatygt24zmd8jrr2vqtvdfptzjhxkguz2xxx3vs0y07u7 \
DRIP_SIGNER_KEY=0101010101010101010101010101010101010101010101010101010101010101 \
DRIP_MIN_BUDGET=0 \
cargo run --bin ligate-api

# 4. Verify
curl http://localhost:8080/v1/health
curl http://localhost:8080/v1/drip/status
```

`CHAIN_ID=4242` is the numeric chain id used by `ligate-devnet-1` and the localnet genesis we ship in `ligate-chain/devnet/`. The Sovereign SDK demo default is `4321`; do not use it, your txs won't include in the local chain. `LGT_TOKEN_ID` is the canonical `$LGT` token id minted by `devnet/genesis/bank.json`; it's a stable constant, no derivation step needed.

The dev key (`0x01...01`) is the chain's localnet dev keypair — pre-funded with 10000 LGT in `devnet/genesis/bank.json`. Don't use it on devnet/testnet/mainnet.

## Deploy: Railway

`railway.toml` pins the build + run steps. To deploy:

1. Connect this repo to a Railway project (Settings → Connect GitHub).
2. Add a Postgres plugin to the project. `DATABASE_URL` auto-wires.
3. Set the chain-side env vars (`CHAIN_RPC`, `CHAIN_ID`, `CHAIN_HASH`, `LGT_TOKEN_ID`).
4. Set `DRIP_SIGNER_KEY` as a Secret-type variable (NOT plain).
5. Optional: `DRIP_AMOUNT`, `DRIP_RATE_LIMIT_SECS`, `DRIP_MIN_BUDGET` to override defaults.
6. Push to `main` → Railway builds and deploys via Dockerfile.
7. Set the public domain to `api.ligate.io` in Railway's Custom Domain settings.

Railway provisions Postgres in the same region as the service; the connection is over Railway's internal network (sub-millisecond).

## Workspace layout

```
ligate-api/
├── Cargo.toml             workspace manifest
├── Dockerfile             multi-stage builder + slim runtime
├── railway.toml           Railway deploy config
├── migrations/            sqlx migrations (Postgres schema)
├── crates/
│   ├── api/               binary; axum router + state composition
│   ├── drip/              faucet primitives (Signer, RateLimiter)
│   ├── indexer/           chain → Postgres ingest task + types
│   └── types/             shared serde types mirroring chain REST
├── constants.toml         Sov-SDK macro anchor (mirror of chain repo)
└── rust-toolchain.toml    1.93.0 pin
```

## Development

```bash
pnpm install                           # actually no, this is Rust — just cargo
cargo fmt --all                        # format
cargo clippy --all-targets             # lint
cargo test                             # tests
cargo build --release --bin ligate-api # production build (locally)
```

CI runs all four on every PR, plus a Postgres-backed e2e smoke for the indexer. See `.github/workflows/ci.yml`.

### Pre-commit hooks

`.pre-commit-config.yaml` runs `cargo fmt --check` on every commit so formatting drift is caught locally instead of in CI. One-time setup per clone:

```bash
brew install pre-commit         # or: pip install pre-commit
pre-commit install              # writes .git/hooks/pre-commit
```

Skip the hook for an emergency commit with `git commit --no-verify`; the same check still re-runs in CI.

### Running the e2e indexer smoke locally

The `e2e-indexer` CI job spins up Postgres in a service container, applies migrations, then runs the indexer's ingest loop against a `mockito`-stubbed chain REST surface and asserts rows landed in the DB. To reproduce locally:

```bash
# Start a local Postgres (any flavor — docker, postgres.app, brew services).
# Then apply the migrations and point the test at it:
export DATABASE_URL=postgres://ligate:ligate@localhost:5432/ligate_indexer
for f in $(ls migrations/*.sql | sort); do
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f"
done
cargo test -p ligate-api-indexer --test e2e -- --nocapture
```

The test is skipped (not failed) when `DATABASE_URL` is unset, so plain `cargo test` stays green for the local-without-Postgres flow.

## Status

**Devnet.** `ligate-devnet-1` is live and the full v0 surface above is wired and serving. Faucet (`/v1/drip*`), explorer-facing indexer queries (`/v1/blocks*`, `/v1/txs*`, `/v1/addresses/*`, `/v1/schemas*`, `/v1/attestor-sets*`, `/v1/attestations*`, `/v1/search`), and analytics stats (`/v1/stats/*`) all hit Postgres. Pagination shapes, cache headers, and per-address drip status landed across PRs #44 to #55.

## Related repos

- [`ligate-chain`](https://github.com/ligate-io/ligate-chain) — Sovereign SDK rollup; `ligate-api` consumes its REST surface
- [`ligate-explorer`](https://github.com/ligate-io/ligate-explorer) — Next.js frontend at `explorer.ligate.io`; calls this API
- [`ligate-cli`](https://github.com/ligate-io/ligate-cli) — Rust operator + builder cli; partners install for sign-tx flows
- [`ligate-js`](https://github.com/ligate-io/ligate-js) — TypeScript SDK; partners install for browser/Node integrations

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
