//! Drip primitives: signer + rate-limiter for the LGT faucet.
//!
//! Ported from the standalone `ligate-io/faucet` repo (now archived) into
//! `ligate-api` so the unified API service hosts faucet drips alongside
//! indexer queries on a single domain (`api.ligate.io/v1/drip`).
//!
//! This crate is HTTP-agnostic. The axum router lives in `ligate-api`
//! (the binary crate); we just export the primitives:
//!
//! - [`Signer`] — wraps the chain's `Submitter`, builds + signs +
//!   submits a `bank.transfer` from the faucet's hot key to a recipient.
//! - [`RateLimiter`] — in-memory `(address → last_drip_at)` map with
//!   configurable cooldown window. v0 in-process only; if we ever
//!   horizontally scale the api, swap to a Postgres-backed impl.
//! - [`DripReceipt`] / [`SignerError`] — return + error types.
//!
//! ## Wire-format gotchas
//!
//! All the same gotchas the chain repo `#245` documented apply here:
//! the chain's `POST /v1/sequencer/txs` handler wraps the body in
//! `AuthenticatorInput::Standard(...)` server-side, so we submit the
//! borsh-encoded `Transaction` bytes directly without pre-wrapping.
//! Inclusion confirmation is HTTP polling on `/v1/ledger/txs/{hash}`,
//! not the SDK's WebSocket subscription (which trips a URL-parse bug
//! on non-standard ports — see `ligate-cli#8`).

// Lint policy: ported types from the standalone faucet binary keep
// their original visibility but pre-date a published-doc requirement.
// We don't lint missing-docs here for v0; tighten once the public
// surface is pinned by `ligate-api`'s composition.

mod ratelimit;
mod signer;

pub use ratelimit::{RateCheck, RateLimiter};
pub use signer::{DripReceipt, Signer, SignerError};
