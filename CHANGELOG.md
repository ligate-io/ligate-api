# Changelog

All notable changes to `ligate-api`. Pre-launch; everything sits under `[Unreleased]` until the first tagged release alongside `ligate-devnet-1`.

Format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/). Issue and PR numbers reference [`ligate-io/ligate-api`](https://github.com/ligate-io/ligate-api).

## [Unreleased]

### Added

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
