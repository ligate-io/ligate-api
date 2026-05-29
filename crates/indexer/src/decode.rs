//! Decode the borsh-encoded signed-transaction body the chain now
//! persists (gated on `runner.save_tx_bodies = true`, ligate-chain#551)
//! and exposes as base64 in `LedgerTx.body.data`.
//!
//! ## Why hand-rolled instead of borsh-decoding the real type
//!
//! The authoritative type is `sov_modules_api::Transaction<R, S>`, which
//! is generic over the whole runtime composition + `Spec` and would drag
//! the entire chain workspace plus the pinned Sovereign SDK into the
//! indexer's dependency tree. The indexer (and `ligate-api-types`)
//! deliberately stay decoupled from the chain crates so `ligate-api`
//! builds fast and doesn't move in lockstep with every SDK rev. We only
//! need three fields, two of which sit at a FIXED offset and one at a
//! tail-anchored offset, so a small, fully-validated byte reader is the
//! right tool — and it's snapshot-tested against a real on-chain tx.
//!
//! ## Wire layout (confirmed against a live devnet-3 tx, body 223 B)
//!
//! The chain stores the tx as
//! `AuthenticatorInput::Standard(RawTx(<inner>))`, whose borsh form
//! prepends a 5-byte wrapper to the inner `Transaction`:
//!
//! ```text
//!   [0]         AuthenticatorInput::Standard variant tag = 0x00
//!   [1..5]      u32 LE length of the inner bytes (= raw.len() - 5)
//!   [5..]       inner Transaction::V0 bytes:
//!     [0]       Transaction enum tag = 0x00 (V0)
//!     [1..65]   signature                          (64, ed25519)
//!     [65..97]  pub_key                            (32, ed25519)
//!     [97..X]   runtime_call                       (variable)
//!     [X..X+9]  uniqueness  tag(1) + u64 nonce LE(8)   (Nonce tag = 0x00)
//!     details:  max_priority_fee_bips u64(8)
//!               + max_fee u128(16)
//!               + gas_limit Option<Gas>(1 [+ Gas])  (None tag = 0x00)
//!               + chain_id u64(8)
//! ```
//!
//! The wrapper is detected and stripped; a bare inner `Transaction`
//! (no wrapper) is also handled.
//!
//! ## What we recover
//!
//! - **`sender_pubkey`** (`lpk1…`): `inner[65..97]`, a fixed offset, so
//!   always exact. Bech32m with HRP `lpk`.
//! - **`sender`** (`lig1…`): the chain's Ed25519 `credential_id` is the
//!   raw 32-byte pubkey (NOT hashed — see the SDK's
//!   `Ed25519PublicKey::credential_id`), and a standard `Address` is its
//!   first 28 bytes. So `sender = bech32m("lig", pub_key[0..28])`.
//!   Cross-checked against the on-chain `Bank/TokenTransferred` `from`.
//! - **`nonce`**: sits after the variable-length `runtime_call`, so we
//!   parse the tail backward, anchored on the known numeric `chain_id`
//!   (the last 8 bytes) plus the `gas_limit` / `uniqueness` tag bytes.
//!   Any validation miss yields `nonce = None` — we never write a
//!   guessed nonce.

use bech32::{Bech32m, Hrp};

/// HRP for `lig1…` addresses.
const HRP_ADDRESS: &str = "lig";
/// HRP for `lpk1…` ed25519 pubkeys.
const HRP_PUBKEY: &str = "lpk";

/// `Transaction::V0` borsh enum tag (also the `AuthenticatorInput::Standard`
/// variant tag, conveniently the same value).
const V0_TAG: u8 = 0x00;
/// ed25519 signature width.
const SIG_LEN: usize = 64;
/// ed25519 pubkey width.
const PUBKEY_LEN: usize = 32;
/// Standard `Address` width = first 28 bytes of the pubkey.
const ADDRESS_LEN: usize = 28;
/// Offset of `pub_key` inside the inner `Transaction::V0`: tag(1) + sig(64).
const INNER_PUBKEY_START: usize = 1 + SIG_LEN;
/// End offset of `pub_key` inside the inner `Transaction::V0`.
const INNER_PUBKEY_END: usize = INNER_PUBKEY_START + PUBKEY_LEN;

/// Width of the 5-byte `AuthenticatorInput::Standard(RawTx)` wrapper:
/// 1-byte variant tag + 4-byte u32 length prefix.
const WRAPPER_LEN: usize = 5;

// Tail (`TxDetails` + `UniquenessData`) field widths.
const CHAIN_ID_LEN: usize = 8; // chain_id: u64
const MAX_FEE_LEN: usize = 16; // max_fee: Amount (u128)
const PRIORITY_FEE_LEN: usize = 8; // max_priority_fee_bips: u64
const UNIQ_NONCE_LEN: usize = 8; // UniquenessData::Nonce(u64)
const UNIQ_TAG_NONCE: u8 = 0x00; // UniquenessData::Nonce variant
const GAS_NONE_TAG: u8 = 0x00; // Option<Gas>::None
const GAS_SOME_TAG: u8 = 0x01; // Option<Gas>::Some
/// Serialized width of `S::Gas` when `gas_limit` is `Some`. The rollup's
/// gas array is `[u64; 2]` (matches the `gas_used: [u64, u64]` shape on
/// batch/tx receipts), so 16 bytes. Only relevant to the defensive
/// `Some` branch; every observed client sends `gas_limit = None`.
const GAS_SOME_WIDTH: usize = 16;

/// Fields recovered from a persisted tx body. Returned by
/// [`decode_tx_body`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTxBody {
    /// Signer address, bech32m `lig1…` = `pub_key[0..28]`.
    pub sender: String,
    /// Signer pubkey, bech32m `lpk1…` (full 32 bytes).
    pub sender_pubkey: String,
    /// Sequential account nonce (`UniquenessData::Nonce`). `None` when
    /// the tail couldn't be validated (fail-safe: never a guessed value).
    pub nonce: Option<u64>,
}

/// Decode a base64 `LedgerTx.body.data` into the signer + nonce.
///
/// Returns `None` for an empty or malformed body — e.g. txs ingested
/// before `save_tx_bodies` was enabled, whose body the chain elided to
/// an empty string. The caller writes NULLs for those, exactly as
/// before this decoder existed.
///
/// `expected_chain_id` is the rollup's numeric chain id (the `CHAIN_ID`
/// the api is configured with). It anchors the tail parse so a wrong
/// guess can't slip through; on mismatch the `sender` / `sender_pubkey`
/// are still returned (they're front-anchored and don't depend on it)
/// but `nonce` is `None`.
pub fn decode_tx_body(body_b64: &str, expected_chain_id: u64) -> Option<DecodedTxBody> {
    let body_b64 = body_b64.trim();
    if body_b64.is_empty() {
        return None;
    }
    let raw = base64_decode(body_b64)?;
    let inner = strip_authenticator_wrapper(&raw);

    // inner[0] must be the Transaction::V0 enum tag, and there must be
    // enough bytes for tag + signature + pubkey.
    if inner.first().copied() != Some(V0_TAG) || inner.len() < INNER_PUBKEY_END {
        return None;
    }

    let pubkey = &inner[INNER_PUBKEY_START..INNER_PUBKEY_END];
    let sender = encode_bech32m(HRP_ADDRESS, &pubkey[..ADDRESS_LEN])?;
    let sender_pubkey = encode_bech32m(HRP_PUBKEY, pubkey)?;
    let nonce = parse_nonce(inner, expected_chain_id);

    Some(DecodedTxBody {
        sender,
        sender_pubkey,
        nonce,
    })
}

/// Strip the `AuthenticatorInput::Standard(RawTx(Vec<u8>))` wrapper if
/// present, returning the inner `Transaction` bytes. Detection is
/// strict — variant tag `0x00`, a u32 length prefix that exactly equals
/// the remaining byte count, and an inner `Transaction::V0` tag — so a
/// bare (unwrapped) transaction whose signature happens to start with
/// `0x00` is not mistaken for a wrapper.
fn strip_authenticator_wrapper(raw: &[u8]) -> &[u8] {
    if raw.len() > WRAPPER_LEN
        && raw[0] == V0_TAG
        && u32::from_le_bytes([raw[1], raw[2], raw[3], raw[4]]) as usize == raw.len() - WRAPPER_LEN
        && raw[WRAPPER_LEN] == V0_TAG
    {
        &raw[WRAPPER_LEN..]
    } else {
        raw
    }
}

/// Parse the `UniquenessData::Nonce(u64)` from the tail of the inner
/// `Transaction`, anchored on the known `chain_id`. Tries `gas_limit =
/// None` (the universal case for current clients) first, then a
/// defensive `Some([u64; 2])` fallback. Returns `None` on any
/// validation miss so a malformed or unexpected layout never yields a
/// bogus nonce.
fn parse_nonce(inner: &[u8], expected_chain_id: u64) -> Option<u64> {
    let m = inner.len();
    if m < CHAIN_ID_LEN {
        return None;
    }
    // chain_id is the last 8 bytes regardless of the gas_limit shape.
    let chain_id = u64::from_le_bytes(inner[m - CHAIN_ID_LEN..].try_into().ok()?);
    if chain_id != expected_chain_id {
        return None;
    }

    // `uniqueness` (tag + nonce) sits immediately before `details`.
    const UNIQ_TOTAL: usize = 1 + UNIQ_NONCE_LEN;
    for gas_field_len in [1usize, 1 + GAS_SOME_WIDTH] {
        let expect_gas_tag = if gas_field_len == 1 {
            GAS_NONE_TAG
        } else {
            GAS_SOME_TAG
        };
        let details_len = PRIORITY_FEE_LEN + MAX_FEE_LEN + gas_field_len + CHAIN_ID_LEN;
        // Start of the gas_limit field (where its Option tag lives).
        let Some(gas_tag_off) = m.checked_sub(CHAIN_ID_LEN + gas_field_len) else {
            continue;
        };
        // Start of the uniqueness field (its variant tag).
        let Some(uniq_tag_off) = m.checked_sub(details_len + UNIQ_TOTAL) else {
            continue;
        };
        // The uniqueness tag must land after the pubkey (i.e. somewhere
        // in the runtime_call region), else the body is too short for
        // this gas shape.
        if uniq_tag_off < INNER_PUBKEY_END {
            continue;
        }
        if inner.get(gas_tag_off).copied() != Some(expect_gas_tag) {
            continue;
        }
        if inner.get(uniq_tag_off).copied() != Some(UNIQ_TAG_NONCE) {
            continue;
        }
        let nonce_off = uniq_tag_off + 1;
        let nonce = u64::from_le_bytes(
            inner[nonce_off..nonce_off + UNIQ_NONCE_LEN]
                .try_into()
                .ok()?,
        );
        return Some(nonce);
    }
    None
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

fn encode_bech32m(hrp_str: &str, data: &[u8]) -> Option<String> {
    let hrp = Hrp::parse(hrp_str).ok()?;
    bech32::encode::<Bech32m>(hrp, data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Real devnet-3 tx #12 (`ltx1xrtsh4z…`), captured live from
    /// rpc.ligate.io. A `bank.transfer`; its decoded signer was
    /// cross-checked byte-for-byte against the on-chain
    /// `Bank/TokenTransferred` `from.user`.
    const GOLDEN_BODY_B64: &str = "ANoAAAAAMNcxnDmZ4COIN+sWi8JhIqkBoIPiYsDfkRNEJG7hkLE/T8op1N9kJF/+AuKdztskN7tENOeo5HTiZPO/5eQEDLJ6BPDZkJaUfO+LI+o2XYXXODStBJ+0kj41RfEU4K5HAAEAomT/izZ/fCaEMKCtqDR4VjpkdpsvJZlHjZvc5wDodkgXAAAAAAAAAAAAAACZPvy8jsj6hMOrIhaqi208hjUwC2NSFYpXNZHBKMY0WQAFAAAAAAAAAAAAAAAAAAAAAOH1BQAAAAAAAAAAAAAAAACSEAAAAAAAAA==";
    const GOLDEN_CHAIN_ID: u64 = 4242;
    const GOLDEN_SENDER: &str = "lig1kfaqfuxejztfgl803v375djashtnsd9dqj0mfy37x4zlzd3nyqm";
    const GOLDEN_SENDER_PUBKEY: &str =
        "lpk1kfaqfuxejztfgl803v375djashtnsd9dqj0mfy37x4zlz98q4ersyjqskt";
    const GOLDEN_NONCE: u64 = 5;

    #[test]
    fn decodes_real_wrapped_tx() {
        let d = decode_tx_body(GOLDEN_BODY_B64, GOLDEN_CHAIN_ID).expect("decodes");
        assert_eq!(d.sender, GOLDEN_SENDER);
        assert_eq!(d.sender_pubkey, GOLDEN_SENDER_PUBKEY);
        assert_eq!(d.nonce, Some(GOLDEN_NONCE));
    }

    #[test]
    fn decodes_bare_unwrapped_tx() {
        // Strip the 5-byte AuthenticatorInput wrapper and feed the bare
        // inner Transaction. sender/pubkey must be identical; the nonce
        // tail parse must still anchor on chain_id.
        let raw = base64::engine::general_purpose::STANDARD
            .decode(GOLDEN_BODY_B64)
            .unwrap();
        let bare = &raw[WRAPPER_LEN..];
        let bare_b64 = base64::engine::general_purpose::STANDARD.encode(bare);
        let d = decode_tx_body(&bare_b64, GOLDEN_CHAIN_ID).expect("decodes bare");
        assert_eq!(d.sender, GOLDEN_SENDER);
        assert_eq!(d.sender_pubkey, GOLDEN_SENDER_PUBKEY);
        assert_eq!(d.nonce, Some(GOLDEN_NONCE));
    }

    #[test]
    fn sender_is_front_anchored_even_on_chain_id_mismatch() {
        // A wrong expected chain id must not corrupt sender/pubkey (they
        // don't depend on it) but must null the nonce (anchor failed).
        let d = decode_tx_body(GOLDEN_BODY_B64, 9999).expect("decodes");
        assert_eq!(d.sender, GOLDEN_SENDER);
        assert_eq!(d.sender_pubkey, GOLDEN_SENDER_PUBKEY);
        assert_eq!(d.nonce, None);
    }

    #[test]
    fn empty_body_is_none() {
        assert!(decode_tx_body("", GOLDEN_CHAIN_ID).is_none());
        assert!(decode_tx_body("   ", GOLDEN_CHAIN_ID).is_none());
    }

    #[test]
    fn non_base64_is_none() {
        assert!(decode_tx_body("not valid base64 !!!", GOLDEN_CHAIN_ID).is_none());
    }

    #[test]
    fn truncated_body_is_none() {
        // Valid base64 but far too short to be a V0 envelope.
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert!(decode_tx_body(&short, GOLDEN_CHAIN_ID).is_none());
    }

    #[test]
    fn wrong_version_tag_is_none() {
        // A buffer long enough but with a non-V0 leading tag.
        let mut bytes = vec![0xFFu8; 200];
        bytes[0] = 0x07;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(decode_tx_body(&b64, GOLDEN_CHAIN_ID).is_none());
    }
}
