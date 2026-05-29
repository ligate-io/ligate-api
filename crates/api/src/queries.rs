//! Read-side sqlx queries for the v1 indexer endpoints.
//!
//! The indexer task (`ligate-api-indexer`) writes to the same tables
//! these functions read from. Splitting reads and writes across two
//! modules keeps the responsibilities clear — the api crate owns the
//! response-shape mapping, the indexer crate owns the ingest pipeline.
//!
//! All queries return Postgres-shaped types (string hashes,
//! `chrono::DateTime<Utc>` timestamps, raw `i64` heights). The
//! handler layer converts to the wire-format types in
//! [`crate::responses`] before serialising — that's where RFC 0002's
//! "RFC3339 with milliseconds", "u128 as decimal string", etc. live.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

/// One row of the `slots` table, mapped to a Rust shape. Mirrors the
/// table definition in `migrations/20260507000001_init.sql`. The
/// handler layer converts this to [`crate::responses::BlockResponse`].
#[derive(Debug)]
pub struct SlotRow {
    pub height: i64,
    pub hash: String,
    pub prev_hash: Option<String>,
    pub state_root: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub batch_count: i32,
    pub tx_count: i32,
    /// Block producer identity. For v0 this is the Celestia
    /// `da_address` of the sequencer that submitted the slot's
    /// first batch to DA (see indexer's `extract_slot_proposer`).
    /// `None` on legacy rows pre-migration-0006 that haven't been
    /// re-ingested, and on slots whose first-batch fetch failed.
    pub proposer: Option<String>,
    /// DA finality state: `Some("pending")`, `Some("finalized")`,
    /// or `None` (legacy rows; chain rev that didn't emit the
    /// field). Frontend treats `None` as "unknown" — render no
    /// badge.
    pub finality_status: Option<String>,
    /// Wall-clock instant the indexer observed the
    /// pending → finalized transition. `None` for currently-pending
    /// slots and for legacy rows where the transition happened
    /// before we tracked it. See indexer's `repoll_pending_loop`
    /// for how this is populated.
    pub finalized_at: Option<DateTime<Utc>>,
    /// Celestia (DA) block height where this slot's first batch
    /// landed. Source: indexer's `extract_slot_first_batch_facts`,
    /// reading `receipt.da_block_height` from the chain JSON (chain
    /// v0.2.3+, ligate-io/ligate-chain#355). `None` on rows ingested
    /// before chain v0.2.3 (no backfill yet) or whose first-batch
    /// fetch failed. Powers the explorer's Celenium deep-link.
    pub da_block_height: Option<i64>,
}

/// Read the highest slot height the indexer has written. `None` for
/// fresh boots that have ingested nothing yet.
pub async fn max_slot_height(pool: &PgPool) -> sqlx::Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as("SELECT MAX(height) FROM slots")
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|(h,)| h))
}

/// Read one slot by its height. `None` when the row doesn't exist
/// yet (indexer hasn't caught up to that height, or the height is
/// above the chain's head).
pub async fn slot_by_height(pool: &PgPool, height: i64) -> sqlx::Result<Option<SlotRow>> {
    let row = sqlx::query_as::<_, SlotTuple>(
        "SELECT height, hash, prev_hash, state_root, timestamp,
                batch_count, tx_count, proposer, finality_status, finalized_at, da_block_height
         FROM slots WHERE height = $1",
    )
    .bind(height)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(slot_row_from_tuple))
}

/// Tuple shape returned by every `SELECT … FROM slots` in this
/// module. Defined once so the inline `sqlx::query_as::<_, …>`
/// generics stay readable as columns grow. Field order MUST match
/// the column order in the SELECT statements.
#[allow(clippy::type_complexity)]
type SlotTuple = (
    i64,                   // height
    String,                // hash
    Option<String>,        // prev_hash
    Option<String>,        // state_root
    DateTime<Utc>,         // timestamp
    i32,                   // batch_count
    i32,                   // tx_count
    Option<String>,        // proposer
    Option<String>,        // finality_status
    Option<DateTime<Utc>>, // finalized_at
    Option<i64>,           // da_block_height
);

fn slot_row_from_tuple(t: SlotTuple) -> SlotRow {
    let (
        height,
        hash,
        prev_hash,
        state_root,
        timestamp,
        batch_count,
        tx_count,
        proposer,
        finality_status,
        finalized_at,
        da_block_height,
    ) = t;
    SlotRow {
        height,
        hash,
        prev_hash,
        state_root,
        timestamp,
        batch_count,
        tx_count,
        proposer,
        finality_status,
        finalized_at,
        da_block_height,
    }
}

/// Read a page of slots, descending by height. `before_height` is the
/// cursor; `None` starts at the head. Fetches `limit + 1` rows so the
/// caller can tell whether a `next` cursor is warranted.
///
/// The `limit + 1` trick avoids a separate `COUNT(*)` or `HAS_MORE`
/// query: if we asked for 20 rows and got 21, there's at least one
/// more page; the 21st row tells us its height (the next page's
/// starting cursor).
pub async fn slots_page(
    pool: &PgPool,
    before_height: Option<i64>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<SlotRow>> {
    // Two query variants instead of one parameterised `$1::bigint`
    // pseudo-NULL because Postgres treats `height < NULL` as
    // unknown (not true), which silently filters out every row.
    // Splitting keeps the planner honest and the SQL readable.
    let rows: Vec<SlotTuple> = match before_height {
        Some(h) => {
            sqlx::query_as(
                "SELECT height, hash, prev_hash, state_root, timestamp,
                        batch_count, tx_count, proposer, finality_status, finalized_at, da_block_height
                 FROM slots
                 WHERE height < $1
                 ORDER BY height DESC
                 LIMIT $2",
            )
            .bind(h)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT height, hash, prev_hash, state_root, timestamp,
                        batch_count, tx_count, proposer, finality_status, finalized_at, da_block_height
                 FROM slots
                 ORDER BY height DESC
                 LIMIT $1",
            )
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.into_iter().map(slot_row_from_tuple).collect())
}

// ---- transactions ----------------------------------------------------------
//
// The `transactions` table is the indexer ingest target — see
// `crates/indexer/src/db.rs::insert_transaction`. These reads join
// against `slots` so a single query returns the block-side fields
// (`block_hash`, `block_timestamp`) the wire shape needs without a
// follow-up roundtrip.

/// One row of the `transactions ⨝ slots` join, mapped to a Rust
/// shape. The handler converts this to [`crate::responses::TxResponse`].
#[derive(Debug)]
pub struct TxRow {
    pub hash: String,
    pub slot: i64,
    pub position: i32,
    pub sender: Option<String>,
    pub sender_pubkey: Option<String>,
    pub nonce: Option<i64>,
    /// Postgres `NUMERIC(78,0)` exposed as `String` via `bigdecimal`.
    /// RFC 0002 wants decimal-string at the wire boundary, so we
    /// surface it as `String` here rather than parsing through a
    /// numeric type that loses precision. Always the **gas fee**;
    /// 0 in practice on devnet because `gas_used = [0, 0]` on every
    /// batch receipt observed so far — the chain meters but doesn't
    /// charge for execution in v0. The chain still publishes a non-
    /// zero `gas_price` (e.g. `["7", "7"]` per dimension on
    /// devnet-1's running config), so this column WILL go non-zero
    /// once the chain starts metering real `gas_used`. For the
    /// module-side protocol fee see [`protocol_fee_nano`] below.
    pub fee_paid_nano: Option<String>,
    /// Protocol fee in nano-AVOW, also a decimal string. Distinct
    /// from `fee_paid_nano` (gas): this is the flat per-call-type
    /// module fee. On `devnet-1` per
    /// `chain/devnet-1/genesis/attestation.json`:
    ///
    /// - `register_attestor_set` = 0.05 AVOW (50_000_000 nano)
    /// - `register_schema`       = 0.10 AVOW (100_000_000 nano)
    /// - `submit_attestation`    = 0.0001 AVOW (100_000 nano)
    ///
    /// The chain code's module-level defaults are 100x higher
    /// (10 / 100 / 0.001 AVOW) and would apply if a genesis didn't
    /// override; both `devnet/` and `devnet-1/` genesis overrides
    /// drop to the above values. `None` for bank.transfer (no
    /// protocol fee) and `unknown` kinds.
    pub protocol_fee_nano: Option<String>,
    pub kind: String,
    pub details: Value,
    pub outcome: String,
    pub revert_reason: Option<String>,
    pub block_hash: Option<String>,
    pub block_timestamp: Option<DateTime<Utc>>,
}

/// Read one tx by hash. `None` if the indexer hasn't written that
/// hash yet — either it's pre-finality on the chain or the tx
/// doesn't exist.
pub async fn tx_by_hash(pool: &PgPool, hash: &str) -> sqlx::Result<Option<TxRow>> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            i64,
            i32,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<sqlx::types::BigDecimal>,
            Option<sqlx::types::BigDecimal>,
            String,
            Value,
            String,
            Option<String>,
            Option<String>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                t.fee_paid_nano, t.protocol_fee_nano,
                t.kind, t.details, t.outcome, t.revert_reason,
                s.hash, s.timestamp
         FROM transactions t
         JOIN slots s ON s.height = t.slot
         WHERE t.hash = $1",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(tx_row_from_tuple))
}

/// Cursor shape for `/v1/txs`. Compound (slot desc, position desc)
/// to give a strict-decreasing key for a stable order across reads,
/// even when the indexer is inserting concurrently.
pub struct TxsCursor {
    pub slot: i64,
    pub position: i32,
}

/// Read a page of txs, descending by (slot, position).
///
/// Optional filters compose multiplicatively — all of them can be
/// applied simultaneously:
/// - `kind_filter` narrows to a single `transactions.kind`
///   (e.g. `"transfer"`, `"submit_attestation"`)
/// - `block_height` narrows to a single `transactions.slot`
///   (added per explorer perf brief api#48; lets
///   `/v1/blocks/{N}` detail pages fetch EXACTLY block N's txs in
///   one round-trip instead of `?limit=100` + client-side filter
///   that silently misses blocks > 100-ago)
/// - `before` is the pagination cursor; `None` starts at the head
///
/// Fetches `limit + 1` rows for has-more detection (same trick as
/// `slots_page`).
///
/// Implementation: collapsed from a 4-way `match` dispatch (was
/// already a tight squeeze; adding `block_height` would have made
/// it 8-way) into a single query with `($N::TYPE IS NULL OR ...)`
/// guards. Postgres short-circuits the NULL branch at plan time so
/// the unused predicates don't cost anything.
pub async fn txs_page(
    pool: &PgPool,
    kind_filter: Option<&str>,
    block_height: Option<u64>,
    before: Option<TxsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<TxRow>> {
    #[allow(clippy::type_complexity)]
    type TxTuple = (
        String,
        i64,
        i32,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<sqlx::types::BigDecimal>,
        Option<sqlx::types::BigDecimal>,
        String,
        Value,
        String,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    );
    let (cursor_slot, cursor_position): (Option<i64>, Option<i32>) = match before {
        Some(c) => (Some(c.slot), Some(c.position)),
        None => (None, None),
    };
    let rows: Vec<TxTuple> = sqlx::query_as(
        "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                t.fee_paid_nano, t.protocol_fee_nano,
                t.kind, t.details, t.outcome, t.revert_reason,
                s.hash, s.timestamp
         FROM transactions t
         JOIN slots s ON s.height = t.slot
         WHERE ($1::TEXT   IS NULL OR t.kind = $1)
           AND ($2::BIGINT IS NULL OR t.slot = $2)
           AND ($3::BIGINT IS NULL OR (t.slot, t.position) < ($3, $4))
         ORDER BY t.slot DESC, t.position DESC
         LIMIT $5",
    )
    .bind(kind_filter)
    .bind(block_height.map(|h| h as i64))
    .bind(cursor_slot)
    .bind(cursor_position)
    .bind(limit_plus_one)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(tx_row_from_tuple).collect())
}

/// Read a page of txs WHERE the given address participated in any
/// role — as `sender` for any tx kind, or as `from` / `to` in a
/// transfer's JSONB details. Same wire shape + cursor as
/// [`txs_page`], so callers can use the same pagination helpers.
///
/// Added per explorer perf brief Tier 1.3 (api#48). Backs the address-
/// detail page's "Recent transactions" section, which until this
/// landed was always empty because no endpoint existed to populate it.
///
/// **SQL semantics.** Three conditions OR'd together:
/// 1. `t.sender = $1` — covers attestation calls, schema registrations,
///    attestor-set registrations, and the sender side of transfers
/// 2. `t.kind = 'transfer' AND t.details->>'from' = $1` — explicit
///    transfer-sender match (redundant with (1) for transfers but
///    cheap, and defensive against indexer rev-drift where `sender`
///    might not always be backfilled correctly for old transfers)
/// 3. `t.kind = 'transfer' AND t.details->>'to' = $1` — covers
///    receiving address (not captured by `sender`)
///
/// JSONB `->>` returns text, which is what we want for bech32m string
/// equality. Indexed implicitly by the `transactions_details_gin`
/// index if it exists; for v0 the address space is small enough that
/// a sequential scan is acceptable.
pub async fn address_txs_page(
    pool: &PgPool,
    addr: &str,
    before: Option<TxsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<TxRow>> {
    #[allow(clippy::type_complexity)]
    type TxTuple = (
        String,
        i64,
        i32,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<sqlx::types::BigDecimal>,
        Option<sqlx::types::BigDecimal>,
        String,
        Value,
        String,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    );
    let (cursor_slot, cursor_position): (Option<i64>, Option<i32>) = match before {
        Some(c) => (Some(c.slot), Some(c.position)),
        None => (None, None),
    };
    let rows: Vec<TxTuple> = sqlx::query_as(
        "SELECT t.hash, t.slot, t.position, t.sender, t.sender_pubkey, t.nonce,
                t.fee_paid_nano, t.protocol_fee_nano,
                t.kind, t.details, t.outcome, t.revert_reason,
                s.hash, s.timestamp
         FROM transactions t
         JOIN slots s ON s.height = t.slot
         WHERE (
             t.sender = $1
             OR (t.kind = 'transfer' AND t.details->>'from' = $1)
             OR (t.kind = 'transfer' AND t.details->>'to'   = $1)
         )
         AND ($2::BIGINT IS NULL OR (t.slot, t.position) < ($2, $3))
         ORDER BY t.slot DESC, t.position DESC
         LIMIT $4",
    )
    .bind(addr)
    .bind(cursor_slot)
    .bind(cursor_position)
    .bind(limit_plus_one)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(tx_row_from_tuple).collect())
}

// ---- schemas ---------------------------------------------------------------
//
// `/v1/schemas` (list) and `/v1/schemas/{id}` (single). All reads
// here only consult the `schemas` table — the registering tx's hash
// is already denormalised onto each row at insert time, so the
// `registered_at_*` fields don't need a join.

/// One row of `schemas`. Handler maps to [`crate::responses::SchemaResponse`].
#[derive(Debug)]
pub struct SchemaRow {
    pub id: String,
    pub name: String,
    pub version: i32,
    pub owner: String,
    pub attestor_set_id: String,
    pub fee_routing_bps: i32,
    pub fee_routing_addr: Option<String>,
    pub payload_shape_hash: String,
    pub registered_at_slot: i64,
    pub registered_at_tx: String,
    pub registered_at_timestamp: DateTime<Utc>,
    pub attestation_count: i32,
    /// Quorum threshold of the bound attestor set, joined in at read
    /// time. Lets the explorer render "M of N" in the schema list
    /// without N+1 fetches per row (Tier 3.2 of explorer perf brief
    /// api#48). `1-64` per the `attestor_sets_threshold_range` CHECK
    /// constraint, so `u8` on the wire shape is sufficient.
    pub threshold: i32,
}

/// Read one schema by id (`lsc1...`). `None` if not yet indexed.
/// Tuple shape returned by every `schemas + attestor_sets` join in
/// this module. Defined once so the `sqlx::query_as::<_, …>` generics
/// stay readable. Field order MUST match the SELECT column order.
///
/// The trailing `i32` is `attestor_sets.threshold`, joined in for
/// the explorer to render "M of N" in the schema list without an
/// N+1 fetch per row (Tier 3.2 of api#48).
#[allow(clippy::type_complexity)]
type SchemaTuple = (
    String,         // s.id
    String,         // s.name
    i32,            // s.version
    String,         // s.owner
    String,         // s.attestor_set_id
    i32,            // s.fee_routing_bps
    Option<String>, // s.fee_routing_addr
    String,         // s.payload_shape_hash
    i64,            // s.registered_at_slot
    String,         // s.registered_at_tx
    DateTime<Utc>,  // s.registered_at_timestamp
    i32,            // s.attestation_count
    i32,            // a.threshold (JOIN attestor_sets)
);

pub async fn schema_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<SchemaRow>> {
    let row = sqlx::query_as::<_, SchemaTuple>(
        "SELECT s.id, s.name, s.version, s.owner, s.attestor_set_id, s.fee_routing_bps,
                s.fee_routing_addr, s.payload_shape_hash,
                s.registered_at_slot, s.registered_at_tx, s.registered_at_timestamp,
                s.attestation_count,
                a.threshold
         FROM schemas s
         JOIN attestor_sets a ON a.id = s.attestor_set_id
         WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(schema_row_from_tuple))
}

/// Cursor shape for `/v1/schemas` (compound: (registered_at_slot
/// DESC, id DESC)). Decoupling slot from id breaks ties when two
/// schemas register in the same slot.
pub struct SchemasCursor {
    pub registered_at_slot: i64,
    pub id: String,
}

/// Read a page of schemas, descending by (registered_at_slot, id).
///
/// Optional filters compose multiplicatively:
/// - `attestor_set_filter` narrows to schemas bound to a single
///   attestor set id (Tier 1.2 of api#48 — powers the
///   `/attestor-set/{id}` detail page's "Bound schemas" section)
/// - `before` is the pagination cursor; `None` starts at the head
///
/// Same `($N::TYPE IS NULL OR ...)` collapse pattern as `txs_page`:
/// optional filters are inert when None is bound, no dispatch
/// explosion, easy to add more filters later (per-name, per-owner
/// etc).
pub async fn schemas_page(
    pool: &PgPool,
    attestor_set_filter: Option<&str>,
    before: Option<SchemasCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<SchemaRow>> {
    let (cursor_slot, cursor_id): (Option<i64>, Option<String>) = match before {
        Some(c) => (Some(c.registered_at_slot), Some(c.id)),
        None => (None, None),
    };
    let rows: Vec<SchemaTuple> = sqlx::query_as(
        "SELECT s.id, s.name, s.version, s.owner, s.attestor_set_id, s.fee_routing_bps,
                s.fee_routing_addr, s.payload_shape_hash,
                s.registered_at_slot, s.registered_at_tx, s.registered_at_timestamp,
                s.attestation_count,
                a.threshold
         FROM schemas s
         JOIN attestor_sets a ON a.id = s.attestor_set_id
         WHERE ($1::TEXT   IS NULL OR s.attestor_set_id = $1)
           AND ($2::BIGINT IS NULL OR (s.registered_at_slot, s.id) < ($2, $3))
         ORDER BY s.registered_at_slot DESC, s.id DESC
         LIMIT $4",
    )
    .bind(attestor_set_filter)
    .bind(cursor_slot)
    .bind(cursor_id)
    .bind(limit_plus_one)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(schema_row_from_tuple).collect())
}

fn schema_row_from_tuple(t: SchemaTuple) -> SchemaRow {
    SchemaRow {
        id: t.0,
        name: t.1,
        version: t.2,
        owner: t.3,
        attestor_set_id: t.4,
        fee_routing_bps: t.5,
        fee_routing_addr: t.6,
        payload_shape_hash: t.7,
        registered_at_slot: t.8,
        registered_at_tx: t.9,
        registered_at_timestamp: t.10,
        attestation_count: t.11,
        threshold: t.12,
    }
}

// ---- attestor_sets ---------------------------------------------------------

/// One row of `attestor_sets`. Handler maps to
/// [`crate::responses::AttestorSetResponse`].
#[derive(Debug)]
pub struct AttestorSetRow {
    pub id: String,
    /// JSONB array of bech32m `lpk1...` member strings. Stays as
    /// `Value` here so the handler can pass it through without a
    /// per-row vec allocation.
    pub members: Value,
    pub threshold: i32,
    pub registered_at_slot: i64,
    pub registered_at_tx: String,
    pub registered_at_timestamp: DateTime<Utc>,
    pub schema_count: i32,
}

/// Read one attestor set by id (`las1...`). `None` if not yet indexed.
pub async fn attestor_set_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<AttestorSetRow>> {
    let row = sqlx::query_as::<_, (String, Value, i32, i64, String, DateTime<Utc>, i32)>(
        "SELECT id, members, threshold,
                registered_at_slot, registered_at_tx, registered_at_timestamp,
                schema_count
         FROM attestor_sets
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|t| AttestorSetRow {
        id: t.0,
        members: t.1,
        threshold: t.2,
        registered_at_slot: t.3,
        registered_at_tx: t.4,
        registered_at_timestamp: t.5,
        schema_count: t.6,
    }))
}

// ---- address_summaries -----------------------------------------------------

/// One row of `address_summaries`, mapped to a Rust shape. The
/// handler converts this to [`crate::responses::AddressSummaryResponse`]
/// after augmenting with chain-side balances.
#[derive(Debug)]
pub struct AddressSummaryRow {
    pub txs_sent_count: i64,
    pub txs_received_count: i64,
    pub first_seen_slot: Option<i64>,
    pub first_seen_timestamp: Option<DateTime<Utc>>,
    pub last_seen_slot: Option<i64>,
    pub last_seen_timestamp: Option<DateTime<Utc>>,
    pub schemas_owned_count: i32,
    pub attestor_member_count: i32,
}

/// Read the summary row for one address. Returns a zeroed-out row
/// (not `None`) when the address has no observed activity — partners
/// asking about a fresh address get a coherent shape with zeros
/// rather than a 404.
pub async fn address_summary(pool: &PgPool, address: &str) -> sqlx::Result<AddressSummaryRow> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        i64,
        i64,
        Option<i64>,
        Option<DateTime<Utc>>,
        Option<i64>,
        Option<DateTime<Utc>>,
        i32,
        i32,
    )> = sqlx::query_as(
        "SELECT txs_sent_count, txs_received_count,
                first_seen_slot, first_seen_timestamp,
                last_seen_slot,  last_seen_timestamp,
                schemas_owned_count, attestor_member_count
         FROM address_summaries
         WHERE address = $1",
    )
    .bind(address)
    .fetch_optional(pool)
    .await?;

    Ok(row
        .map(|t| AddressSummaryRow {
            txs_sent_count: t.0,
            txs_received_count: t.1,
            first_seen_slot: t.2,
            first_seen_timestamp: t.3,
            last_seen_slot: t.4,
            last_seen_timestamp: t.5,
            schemas_owned_count: t.6,
            attestor_member_count: t.7,
        })
        .unwrap_or_else(|| AddressSummaryRow {
            txs_sent_count: 0,
            txs_received_count: 0,
            first_seen_slot: None,
            first_seen_timestamp: None,
            last_seen_slot: None,
            last_seen_timestamp: None,
            schemas_owned_count: 0,
            attestor_member_count: 0,
        }))
}

#[allow(clippy::type_complexity)]
fn tx_row_from_tuple(
    t: (
        String,
        i64,
        i32,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<sqlx::types::BigDecimal>,
        Option<sqlx::types::BigDecimal>,
        String,
        Value,
        String,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    ),
) -> TxRow {
    let (
        hash,
        slot,
        position,
        sender,
        sender_pubkey,
        nonce,
        fee_paid_nano,
        protocol_fee_nano,
        kind,
        details,
        outcome,
        revert_reason,
        block_hash,
        block_timestamp,
    ) = t;
    TxRow {
        hash,
        slot,
        position,
        sender,
        sender_pubkey,
        nonce,
        // BigDecimal → String. Trimmed of trailing decimal noise so
        // a `1000000000` row comes back as `"1000000000"`, not
        // `"1000000000.0"` (BigDecimal's default Display).
        fee_paid_nano: fee_paid_nano.map(|bd| bd.with_scale(0).to_string()),
        protocol_fee_nano: protocol_fee_nano.map(|bd| bd.with_scale(0).to_string()),
        kind,
        details,
        outcome,
        revert_reason,
        block_hash,
        block_timestamp,
    }
}

// ---- attestations ----------------------------------------------------------

/// One row of `attestations` plus the FK-joined registration provenance.
///
/// `id` is the v0.2.0 canonical `lat1...` AttestationId, derived by
/// the indexer at ingest via SHA-256(schema_id_bytes || payload_hash_bytes)
/// and persisted alongside its constituent `schema_id` + `payload_hash`
/// fields. Path routing (`/v1/attestations/{id}`) uses `id`; the
/// constituents are retained in the wire shape so partners that need
/// them don't have to fetch the schema or chain separately.
#[derive(Debug)]
pub struct AttestationRow {
    /// Bech32m `lat1...` AttestationId.
    pub id: String,
    pub schema_id: String,
    /// Bech32m `lph1...` payload hash.
    pub payload_hash: String,
    pub submitter: String,
    /// Nullable per migration 0004 — chain emits `submitter` as
    /// `S::Address` only; the raw pubkey isn't on the event payload,
    /// so partners who need it resolve via the `accounts` module at
    /// read time.
    pub submitter_pubkey: Option<String>,
    pub submitted_at_slot: i64,
    pub submitted_at_tx: String,
    pub submitted_at_timestamp: DateTime<Utc>,
    /// `attestations.signature_count` column (chain enforces this is
    /// `>= schema.threshold`, so it's always populated).
    pub signature_count: i32,
}

/// Cursor shape for `/v1/attestations` (compound:
/// `(submitted_at_slot, schema_id, payload_hash)` all DESC). The
/// payload-hash tiebreaker handles the edge case where two
/// attestations under different schemas land in the same slot.
pub struct AttestationsCursor {
    pub submitted_at_slot: i64,
    pub schema_id: String,
    pub payload_hash: String,
}

/// Read a page of attestations, descending by `(submitted_at_slot,
/// schema_id, payload_hash)`. Optionally filter to a single
/// `schema_id_filter` for `/v1/schemas/{id}/attestations`.
pub async fn attestations_page(
    pool: &PgPool,
    schema_id_filter: Option<&str>,
    before: Option<AttestationsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<AttestationRow>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        DateTime<Utc>,
        i32,
    )> = match (schema_id_filter, before) {
        (Some(s), Some(c)) => {
            sqlx::query_as(
                "SELECT id, schema_id, payload_hash, submitter, submitter_pubkey,
                        submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
                        signature_count
                 FROM attestations
                 WHERE schema_id = $1
                   AND (submitted_at_slot, schema_id, payload_hash) < ($2, $3, $4)
                 ORDER BY submitted_at_slot DESC, schema_id DESC, payload_hash DESC
                 LIMIT $5",
            )
            .bind(s)
            .bind(c.submitted_at_slot)
            .bind(&c.schema_id)
            .bind(&c.payload_hash)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        (Some(s), None) => {
            sqlx::query_as(
                "SELECT id, schema_id, payload_hash, submitter, submitter_pubkey,
                        submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
                        signature_count
                 FROM attestations
                 WHERE schema_id = $1
                 ORDER BY submitted_at_slot DESC, schema_id DESC, payload_hash DESC
                 LIMIT $2",
            )
            .bind(s)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        (None, Some(c)) => {
            sqlx::query_as(
                "SELECT id, schema_id, payload_hash, submitter, submitter_pubkey,
                        submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
                        signature_count
                 FROM attestations
                 WHERE (submitted_at_slot, schema_id, payload_hash) < ($1, $2, $3)
                 ORDER BY submitted_at_slot DESC, schema_id DESC, payload_hash DESC
                 LIMIT $4",
            )
            .bind(c.submitted_at_slot)
            .bind(&c.schema_id)
            .bind(&c.payload_hash)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        (None, None) => {
            sqlx::query_as(
                "SELECT id, schema_id, payload_hash, submitter, submitter_pubkey,
                        submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
                        signature_count
                 FROM attestations
                 ORDER BY submitted_at_slot DESC, schema_id DESC, payload_hash DESC
                 LIMIT $1",
            )
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows.into_iter().map(attestation_row_from_tuple).collect())
}

/// Read a page of attestations filtered to a single attestor-set id.
///
/// Two-hop: an attestor set doesn't directly point at attestations,
/// it points at schemas (via `schemas.attestor_set_id`), and schemas
/// point at attestations (via `attestations.schema_id`). One JOIN
/// stitches them.
pub async fn attestations_by_attestor_set(
    pool: &PgPool,
    attestor_set_id: &str,
    before: Option<AttestationsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<AttestationRow>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        DateTime<Utc>,
        i32,
    )> = match before {
        Some(c) => {
            sqlx::query_as(
                "SELECT a.id, a.schema_id, a.payload_hash, a.submitter, a.submitter_pubkey,
                        a.submitted_at_slot, a.submitted_at_tx, a.submitted_at_timestamp,
                        a.signature_count
                 FROM attestations a
                 JOIN schemas s ON s.id = a.schema_id
                 WHERE s.attestor_set_id = $1
                   AND (a.submitted_at_slot, a.schema_id, a.payload_hash) < ($2, $3, $4)
                 ORDER BY a.submitted_at_slot DESC, a.schema_id DESC, a.payload_hash DESC
                 LIMIT $5",
            )
            .bind(attestor_set_id)
            .bind(c.submitted_at_slot)
            .bind(&c.schema_id)
            .bind(&c.payload_hash)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT a.id, a.schema_id, a.payload_hash, a.submitter, a.submitter_pubkey,
                        a.submitted_at_slot, a.submitted_at_tx, a.submitted_at_timestamp,
                        a.signature_count
                 FROM attestations a
                 JOIN schemas s ON s.id = a.schema_id
                 WHERE s.attestor_set_id = $1
                 ORDER BY a.submitted_at_slot DESC, a.schema_id DESC, a.payload_hash DESC
                 LIMIT $2",
            )
            .bind(attestor_set_id)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows.into_iter().map(attestation_row_from_tuple).collect())
}

/// Read one attestation by its canonical `lat1...` AttestationId.
/// `None` if not yet indexed. v0.2.0 replaced the prior
/// `attestation_by_pair(schema_id, payload_hash)` lookup; the id is
/// the SHA-256 of the pair so callers that still hold the pair can
/// recover the id via the indexer's `compute_attestation_id` helper.
pub async fn attestation_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<AttestationRow>> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        DateTime<Utc>,
        i32,
    )> = sqlx::query_as(
        "SELECT id, schema_id, payload_hash, submitter, submitter_pubkey,
                submitted_at_slot, submitted_at_tx, submitted_at_timestamp,
                signature_count
         FROM attestations
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(attestation_row_from_tuple))
}

#[allow(clippy::type_complexity)]
fn attestation_row_from_tuple(
    t: (
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        DateTime<Utc>,
        i32,
    ),
) -> AttestationRow {
    AttestationRow {
        id: t.0,
        schema_id: t.1,
        payload_hash: t.2,
        submitter: t.3,
        submitter_pubkey: t.4,
        submitted_at_slot: t.5,
        submitted_at_tx: t.6,
        submitted_at_timestamp: t.7,
        signature_count: t.8,
    }
}

// ---- attestor_sets list ----------------------------------------------------

/// Cursor shape for `/v1/attestor-sets` (compound:
/// `(registered_at_slot, id)` DESC). Mirrors `SchemasCursor`.
pub struct AttestorSetsCursor {
    pub registered_at_slot: i64,
    pub id: String,
}

/// Read a page of attestor sets whose `members` JSONB array contains
/// `pubkey` (bech32m `lpk1...`). Same `(registered_at_slot, id)` DESC
/// ordering and cursor shape as [`attestor_sets_page`], so the wire
/// envelope is identical and the handler can reuse the same encode/
/// decode + truncation logic.
///
/// Containment is checked with the JSONB `@>` operator against a
/// single-element array — Postgres uses the GIN index on
/// `attestor_sets.members` (migration
/// `20260509000001_indexer_query_tables.sql:174-176`) so the WHERE
/// is an index seek, not a full table scan.
pub async fn attestor_sets_for_member_page(
    pool: &PgPool,
    pubkey: &str,
    before: Option<AttestorSetsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<AttestorSetRow>> {
    // `members @> [pubkey]` is the JSONB containment query the GIN
    // index serves. Bind via serde_json::Value so sqlx encodes it
    // as JSONB, not text.
    let member_filter = serde_json::json!([pubkey]);
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, Value, i32, i64, String, DateTime<Utc>, i32)> = match before {
        Some(c) => {
            sqlx::query_as(
                "SELECT id, members, threshold,
                        registered_at_slot, registered_at_tx, registered_at_timestamp,
                        schema_count
                 FROM attestor_sets
                 WHERE members @> $1
                   AND (registered_at_slot, id) < ($2, $3)
                 ORDER BY registered_at_slot DESC, id DESC
                 LIMIT $4",
            )
            .bind(&member_filter)
            .bind(c.registered_at_slot)
            .bind(&c.id)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT id, members, threshold,
                        registered_at_slot, registered_at_tx, registered_at_timestamp,
                        schema_count
                 FROM attestor_sets
                 WHERE members @> $1
                 ORDER BY registered_at_slot DESC, id DESC
                 LIMIT $2",
            )
            .bind(&member_filter)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|t| AttestorSetRow {
            id: t.0,
            members: t.1,
            threshold: t.2,
            registered_at_slot: t.3,
            registered_at_tx: t.4,
            registered_at_timestamp: t.5,
            schema_count: t.6,
        })
        .collect())
}

/// Read a page of attestor sets, descending by
/// `(registered_at_slot, id)`. Companion to the existing
/// [`attestor_set_by_id`] for the per-id case.
pub async fn attestor_sets_page(
    pool: &PgPool,
    before: Option<AttestorSetsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<AttestorSetRow>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, Value, i32, i64, String, DateTime<Utc>, i32)> = match before {
        Some(c) => {
            sqlx::query_as(
                "SELECT id, members, threshold,
                        registered_at_slot, registered_at_tx, registered_at_timestamp,
                        schema_count
                 FROM attestor_sets
                 WHERE (registered_at_slot, id) < ($1, $2)
                 ORDER BY registered_at_slot DESC, id DESC
                 LIMIT $3",
            )
            .bind(c.registered_at_slot)
            .bind(&c.id)
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT id, members, threshold,
                        registered_at_slot, registered_at_tx, registered_at_timestamp,
                        schema_count
                 FROM attestor_sets
                 ORDER BY registered_at_slot DESC, id DESC
                 LIMIT $1",
            )
            .bind(limit_plus_one)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|t| AttestorSetRow {
            id: t.0,
            members: t.1,
            threshold: t.2,
            registered_at_slot: t.3,
            registered_at_tx: t.4,
            registered_at_timestamp: t.5,
            schema_count: t.6,
        })
        .collect())
}

// ---- search helpers --------------------------------------------------------

/// Lightweight "does this block-hash exist" check for `/v1/search`.
/// Returns the slot height the hash hashes to, so the search handler
/// can redirect to `/v1/blocks/{height}`. The handler doesn't need
/// the full block row at search time.
pub async fn slot_height_for_block_hash(pool: &PgPool, hash: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar("SELECT height FROM slots WHERE hash = $1")
        .bind(hash)
        .fetch_optional(pool)
        .await
}

/// Lightweight "does this address exist" check for `/v1/search`. We
/// only return `true`/`false` because the search handler just needs
/// to know whether to redirect to `/v1/addresses/{addr}`; the actual
/// summary is fetched on that follow-up call.
///
/// `SELECT EXISTS(...)` rather than `SELECT 1` — same fragile-decode
/// reason called out on `schema_exists` below (sqlx couldn't decode
/// Postgres's `1` int4 literal into the `Option<i64>` we'd previously
/// declared, and the handler 500ed on every `/v1/search?q=lig1…`
/// call). #50 fixed schemas + attestor-sets; this catches addresses,
/// which the smoke pass surfaced 2026-05-16.
pub async fn address_exists(pool: &PgPool, address: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM address_summaries WHERE address = $1)")
        .bind(address)
        .fetch_one(pool)
        .await
}

/// Lightweight "does this schema id exist" check.
///
/// `SELECT EXISTS(...)` rather than `SELECT 1` because Postgres's `1`
/// literal types as `int4` and sqlx is strict about decode types —
/// previously `Option<i64>` triggered a runtime decode error and the
/// handler 500ed on every `/v1/search?q=lsc1…` call. `EXISTS` always
/// returns a clean `bool`, no fragile type casts.
pub async fn schema_exists(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM schemas WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// Lightweight "does this attestor-set id exist" check. Same
/// `EXISTS(...)` pattern as `schema_exists` for the same reason.
pub async fn attestor_set_exists(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM attestor_sets WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// "Does this `lat1...` AttestationId exist?", used by `/v1/search`
/// when the input is a `lat1...` string. v0.2.0 replaced the prior
/// `attestation_pair_exists((schema_id, payload_hash))` helper; the
/// id is the canonical key now. Same `EXISTS(...)` pattern as the
/// other `*_exists` helpers above.
pub async fn attestation_id_exists(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM attestations WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// "Which attestation has this payload hash?", used by `/v1/search`
/// when the input is a `lph1...` (payload hash). Returns the
/// `lat1...` AttestationId of the first match, or `None`. The same
/// payload hash can land under multiple schemas, so this is "first
/// match wins" by `(submitted_at_slot, schema_id)`; callers that need
/// all matches scan `/v1/schemas/{id}/attestations` per schema.
pub async fn attestation_id_by_payload_hash(
    pool: &PgPool,
    payload_hash: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT id FROM attestations
         WHERE payload_hash = $1
         ORDER BY submitted_at_slot DESC, schema_id ASC
         LIMIT 1",
    )
    .bind(payload_hash)
    .fetch_optional(pool)
    .await
}

// ---- bounties --------------------------------------------------------------
//
// `/v1/bounties/matching/{address}` reads from the `bounties` table
// (populated by the indexer on `Bounty/*` chain events; see the
// `20260528000001_bounties.sql` migration). The match logic:
//
// - Filter open bounties (`status='open'`) whose `board_schema_id`
//   appears in the attestations the address has already submitted.
// - For v0, the chain-side acceptance predicate (`Any` /
//   `AttestorSet` / `PayloadHashes` / `PeerCount`) is NOT enforced
//   indexer-side. The matching service surfaces "you have attested
//   against this schema and there's an open bounty for it", letting
//   the wallet decide whether to attempt a `ClaimBounty`. The chain
//   enforces the predicate at ClaimBounty time.
// - Returned tuples carry enough fields to render the Mneme post-
//   attest panel without an N+1 fetch per match: bounty id, board
//   schema id, per_attestation payout, expiry, status, the address's
//   attestation count against the schema.

/// One match row for `/v1/bounties/matching/{address}`.
#[derive(Debug)]
pub struct BountyMatchRow {
    /// Bech32m `lbt1...` bounty id.
    pub id: String,
    /// `lid1...` of the bounty poster.
    pub poster: String,
    /// `lsc1...` of the bounty board's schema.
    pub board_schema_id: String,
    /// Per-attestation payout in AVOW nanos as a decimal string
    /// (preserves u128 precision).
    pub per_attestation_nano: String,
    /// Remaining escrow at the indexer's last seen event.
    pub escrow_remaining_nano: String,
    /// Original pool size at PostBounty time.
    pub pool_nano: String,
    /// Acceptance predicate as compact JSONB; mirrors the chain's
    /// `AcceptancePredicate` enum.
    pub acceptance: Value,
    /// DA-layer block height the bounty expires at.
    pub expiry_da_height: i64,
    /// Slot the bounty was posted at; drives the default ordering.
    pub posted_at_slot: i64,
    /// Count of the candidate address's attestations against the
    /// bounty's board schema. Lets the Mneme UI show "you've already
    /// attested N times against this board" inline.
    pub my_attestation_count: i64,
}

#[allow(clippy::type_complexity)]
type BountyMatchTuple = (
    String, // b.id
    String, // b.poster
    String, // b.board_schema_id
    String, // b.per_attestation_nano
    String, // b.escrow_remaining_nano
    String, // b.pool_nano
    Value,  // b.acceptance
    i64,    // b.expiry_da_height
    i64,    // b.posted_at_slot
    i64,    // count(a.id)
);

/// Read up to `limit` open bounties the address is potentially
/// eligible for: those whose board schema appears in the address's
/// attestations. Ordered by `(posted_at_slot DESC, id ASC)`.
///
/// `address` is the candidate's `lid1...` address.
/// `limit` is clamped 1..=100 by the handler.
pub async fn bounties_matching_address(
    pool: &PgPool,
    address: &str,
    limit: i64,
) -> sqlx::Result<Vec<BountyMatchRow>> {
    let rows = sqlx::query_as::<_, BountyMatchTuple>(
        "SELECT b.id, b.poster, b.board_schema_id,
                b.per_attestation_nano, b.escrow_remaining_nano, b.pool_nano,
                b.acceptance, b.expiry_da_height, b.posted_at_slot,
                COUNT(a.id) AS my_attestation_count
         FROM bounties b
         JOIN attestations a ON a.schema_id = b.board_schema_id
         WHERE b.status = 'open'
           AND a.submitter = $1
         GROUP BY b.id
         ORDER BY b.posted_at_slot DESC, b.id ASC
         LIMIT $2",
    )
    .bind(address)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|t| BountyMatchRow {
            id: t.0,
            poster: t.1,
            board_schema_id: t.2,
            per_attestation_nano: t.3,
            escrow_remaining_nano: t.4,
            pool_nano: t.5,
            acceptance: t.6,
            expiry_da_height: t.7,
            posted_at_slot: t.8,
            my_attestation_count: t.9,
            // `pool_nano` is the column above; surfaced too so the
            // wallet can render "X of Y AVOW remaining" without a
            // second fetch.
        })
        .collect())
}

/// One row of the `bounties` table, mapped to a Rust shape. Mirrors
/// the table definition in `migrations/20260528000001_bounties.sql`.
/// The handler converts this to
/// [`crate::responses::BountyDetailResponse`].
#[derive(Debug)]
pub struct BountyRow {
    /// Bech32m `lbt1...` bounty id.
    pub id: String,
    /// Poster address (the chain emits a raw bech32m `lig1...` string;
    /// stored verbatim).
    pub poster: String,
    /// Bech32m `lsc1...` of the bounty board schema.
    pub board_schema_id: String,
    /// Original pool size in AVOW nanos (u128 decimal string).
    pub pool_nano: String,
    /// Per-accepted-claim payout in AVOW nanos (u128 decimal string).
    pub per_attestation_nano: String,
    /// Remaining escrow at the indexer's last seen event (u128 string).
    pub escrow_remaining_nano: String,
    /// One of `open`/`exhausted`/`expired`/`cancelled`/`finalised`.
    pub status: String,
    /// Acceptance predicate as compact JSONB (pass-through).
    pub acceptance: Value,
    /// DA-layer block height the bounty expires at.
    pub expiry_da_height: i64,
    /// Dispute window in chain blocks.
    pub dispute_window_blocks: i32,
    /// Slot the PostBounty tx landed in.
    pub posted_at_slot: i64,
    /// Tx hash of the PostBounty tx.
    pub posted_at_tx: String,
    /// Timestamp of the PostBounty tx.
    pub posted_at_timestamp: DateTime<Utc>,
    /// Running count of `BountyClaimed` events seen.
    pub claim_count: i32,
    /// Slot of the most recent claim; `None` if never claimed.
    pub last_claim_at_slot: Option<i64>,
}

#[allow(clippy::type_complexity)]
type BountyTuple = (
    String,        // id
    String,        // poster
    String,        // board_schema_id
    String,        // pool_nano
    String,        // per_attestation_nano
    String,        // escrow_remaining_nano
    String,        // status
    Value,         // acceptance
    i64,           // expiry_da_height
    i32,           // dispute_window_blocks
    i64,           // posted_at_slot
    String,        // posted_at_tx
    DateTime<Utc>, // posted_at_timestamp
    i32,           // claim_count
    Option<i64>,   // last_claim_at_slot
);

fn bounty_row_from_tuple(t: BountyTuple) -> BountyRow {
    BountyRow {
        id: t.0,
        poster: t.1,
        board_schema_id: t.2,
        pool_nano: t.3,
        per_attestation_nano: t.4,
        escrow_remaining_nano: t.5,
        status: t.6,
        acceptance: t.7,
        expiry_da_height: t.8,
        dispute_window_blocks: t.9,
        posted_at_slot: t.10,
        posted_at_tx: t.11,
        posted_at_timestamp: t.12,
        claim_count: t.13,
        last_claim_at_slot: t.14,
    }
}

/// The `SELECT` column list shared by [`bounty_by_id`] and
/// [`bounties_page`]. Order MUST match [`BountyTuple`].
const BOUNTY_COLUMNS: &str = "id, poster, board_schema_id, pool_nano, per_attestation_nano,
            escrow_remaining_nano, status, acceptance, expiry_da_height,
            dispute_window_blocks, posted_at_slot, posted_at_tx, posted_at_timestamp,
            claim_count, last_claim_at_slot";

/// Read one bounty by its bech32m `lbt1...` id. `None` if not yet
/// indexed (the indexer hasn't seen a `Bounty/*` event for it, or it
/// doesn't exist).
pub async fn bounty_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<BountyRow>> {
    let sql = format!("SELECT {BOUNTY_COLUMNS} FROM bounties WHERE id = $1");
    let row = sqlx::query_as::<_, BountyTuple>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(bounty_row_from_tuple))
}

/// Cursor shape for `/v1/bounties` (compound: `(posted_at_slot, id)`
/// DESC). The id tiebreaker handles bounties posted in the same slot.
pub struct BountiesCursor {
    pub posted_at_slot: i64,
    pub id: String,
}

/// Read a page of bounties, descending by `(posted_at_slot, id)`.
///
/// Optional filters compose multiplicatively (same
/// `($N::TYPE IS NULL OR ...)` collapse pattern as [`txs_page`] /
/// [`schemas_page`], so the inert filter costs nothing at plan time):
/// - `board` narrows to a single `board_schema_id` (`lsc1...`)
/// - `status` narrows to a single lifecycle state
///   (`open`/`exhausted`/`expired`/`cancelled`/`finalised`)
/// - `before` is the pagination cursor; `None` starts at the head
///
/// Fetches `limit + 1` rows for has-more detection (same trick as the
/// other list queries).
pub async fn bounties_page(
    pool: &PgPool,
    board: Option<&str>,
    status: Option<&str>,
    before: Option<BountiesCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<BountyRow>> {
    let (cursor_slot, cursor_id): (Option<i64>, Option<String>) = match before {
        Some(c) => (Some(c.posted_at_slot), Some(c.id)),
        None => (None, None),
    };
    let sql = format!(
        "SELECT {BOUNTY_COLUMNS}
         FROM bounties
         WHERE ($1::TEXT   IS NULL OR board_schema_id = $1)
           AND ($2::TEXT   IS NULL OR status = $2)
           AND ($3::BIGINT IS NULL OR (posted_at_slot, id) < ($3, $4))
         ORDER BY posted_at_slot DESC, id DESC
         LIMIT $5"
    );
    let rows: Vec<BountyTuple> = sqlx::query_as(&sql)
        .bind(board)
        .bind(status)
        .bind(cursor_slot)
        .bind(cursor_id)
        .bind(limit_plus_one)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(bounty_row_from_tuple).collect())
}

// ---- contracts -------------------------------------------------------------
//
// `/v1/contracts` (list) and `/v1/contracts/{id}` (detail) read from the
// `contracts` table, populated by the indexer on `Contracts/*` chain
// events (see the `20260529000001_contracts.sql` migration). Mirrors the
// bounties read path; the differences are: no board-schema field
// (contracts aren't schema-anchored), an `arbiter` column + `?arbiter=`
// filter, and the 8-state status enum.

/// One row of the `contracts` table, mapped to a Rust shape. Mirrors
/// the table definition in `migrations/20260529000001_contracts.sql`.
/// The handler converts this to
/// [`crate::responses::ContractDetailResponse`].
#[derive(Debug)]
pub struct ContractRow {
    /// Bech32m `lct1...` contract id.
    pub id: String,
    /// Poster address (raw bech32m `lig1...`, stored verbatim).
    pub poster: String,
    /// Arbiter address named at post time (raw bech32m `lig1...`).
    pub arbiter: String,
    /// 32-byte criteria-doc content hash (TEXT, hex pass-through).
    pub criteria_doc_hash: String,
    /// Original pool size in AVOW nanos (u128 decimal string).
    pub pool_nano: String,
    /// Remaining escrow at the indexer's last seen event (u128 string).
    pub escrow_remaining_nano: String,
    /// Arbiter fee in basis points.
    pub arbiter_fee_bps: i32,
    /// One of `open`/`committed`/`delivered`/`accepted`/`rejected`/
    /// `disputed`/`cancelled`/`expired`.
    pub status: String,
    /// DA-layer block height the contract expires at.
    pub expiry_da_height: i64,
    /// Acceptance window in chain blocks before auto-accept.
    pub dispute_window_blocks: i32,
    /// Slot the PostContract tx landed in.
    pub posted_at_slot: i64,
    /// Tx hash of the PostContract tx.
    pub posted_at_tx: String,
    /// Timestamp of the PostContract tx.
    pub posted_at_timestamp: DateTime<Utc>,
}

#[allow(clippy::type_complexity)]
type ContractTuple = (
    String,        // id
    String,        // poster
    String,        // arbiter
    String,        // criteria_doc_hash
    String,        // pool_nano
    String,        // escrow_remaining_nano
    i32,           // arbiter_fee_bps
    String,        // status
    i64,           // expiry_da_height
    i32,           // dispute_window_blocks
    i64,           // posted_at_slot
    String,        // posted_at_tx
    DateTime<Utc>, // posted_at_timestamp
);

fn contract_row_from_tuple(t: ContractTuple) -> ContractRow {
    ContractRow {
        id: t.0,
        poster: t.1,
        arbiter: t.2,
        criteria_doc_hash: t.3,
        pool_nano: t.4,
        escrow_remaining_nano: t.5,
        arbiter_fee_bps: t.6,
        status: t.7,
        expiry_da_height: t.8,
        dispute_window_blocks: t.9,
        posted_at_slot: t.10,
        posted_at_tx: t.11,
        posted_at_timestamp: t.12,
    }
}

/// The `SELECT` column list shared by [`contract_by_id`] and
/// [`contracts_page`]. Order MUST match [`ContractTuple`].
const CONTRACT_COLUMNS: &str = "id, poster, arbiter, criteria_doc_hash, pool_nano,
            escrow_remaining_nano, arbiter_fee_bps, status, expiry_da_height,
            dispute_window_blocks, posted_at_slot, posted_at_tx, posted_at_timestamp";

/// Read one contract by its bech32m `lct1...` id. `None` if not yet
/// indexed (the indexer hasn't seen a `Contracts/*` event for it, or it
/// doesn't exist).
pub async fn contract_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<ContractRow>> {
    let sql = format!("SELECT {CONTRACT_COLUMNS} FROM contracts WHERE id = $1");
    let row = sqlx::query_as::<_, ContractTuple>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(contract_row_from_tuple))
}

/// Cursor shape for `/v1/contracts` (compound: `(posted_at_slot, id)`
/// DESC). The id tiebreaker handles contracts posted in the same slot.
pub struct ContractsCursor {
    pub posted_at_slot: i64,
    pub id: String,
}

/// Read a page of contracts, descending by `(posted_at_slot, id)`.
///
/// Optional filters compose multiplicatively (same
/// `($N::TYPE IS NULL OR ...)` collapse pattern as [`bounties_page`], so
/// an inert filter costs nothing at plan time):
/// - `status` narrows to a single lifecycle state (`open`/`committed`/
///   `delivered`/`accepted`/`rejected`/`disputed`/`cancelled`/`expired`)
/// - `poster` narrows to a single poster address (`lig1...`)
/// - `arbiter` narrows to a single named arbiter address (`lig1...`)
/// - `before` is the pagination cursor; `None` starts at the head
///
/// Fetches `limit + 1` rows for has-more detection (same trick as the
/// other list queries).
pub async fn contracts_page(
    pool: &PgPool,
    status: Option<&str>,
    poster: Option<&str>,
    arbiter: Option<&str>,
    before: Option<ContractsCursor>,
    limit_plus_one: i64,
) -> sqlx::Result<Vec<ContractRow>> {
    let (cursor_slot, cursor_id): (Option<i64>, Option<String>) = match before {
        Some(c) => (Some(c.posted_at_slot), Some(c.id)),
        None => (None, None),
    };
    let sql = format!(
        "SELECT {CONTRACT_COLUMNS}
         FROM contracts
         WHERE ($1::TEXT   IS NULL OR status = $1)
           AND ($2::TEXT   IS NULL OR poster = $2)
           AND ($3::TEXT   IS NULL OR arbiter = $3)
           AND ($4::BIGINT IS NULL OR (posted_at_slot, id) < ($4, $5))
         ORDER BY posted_at_slot DESC, id DESC
         LIMIT $6"
    );
    let rows: Vec<ContractTuple> = sqlx::query_as(&sql)
        .bind(status)
        .bind(poster)
        .bind(arbiter)
        .bind(cursor_slot)
        .bind(cursor_id)
        .bind(limit_plus_one)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(contract_row_from_tuple).collect())
}
