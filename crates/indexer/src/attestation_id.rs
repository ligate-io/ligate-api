//! Derivation of the v0.2.0 [`AttestationId`] (`lat1...`) from the
//! `(schema_id, payload_hash)` pair the chain emits.
//!
//! Mirrors the chain reference at
//! `ligate-chain/crates/modules/attestation/src/lib.rs::AttestationId::from_pair`,
//! and is snapshot-tested against the chain's `borsh_snapshot.rs`
//! vector: `(schema_id = [0x11; 32], payload_hash = [0x33; 32])`
//! SHA-256s to the bytes
//! `b0dcb09af5496e779e60b21109a718475091191efc7a8638b01d51c622fc9128`,
//! which encode as a `lat1...` bech32m string.
//!
//! [`AttestationId`]: https://github.com/ligate-io/ligate-chain/blob/main/crates/modules/attestation/src/lib.rs

use bech32::{Bech32m, Hrp};
use sha2::{Digest, Sha256};

const HRP_SCHEMA: &str = "lsc";
const HRP_PAYLOAD: &str = "lph";
const HRP_ATTESTATION: &str = "lat";

/// Why a `(schema_id, payload_hash)` pair couldn't be collapsed to a
/// canonical [`AttestationId`]. All variants are recoverable: the
/// indexer skips the offending event and moves on, so a single
/// malformed chain event can't wedge the ingest loop.
#[derive(Debug, thiserror::Error)]
pub enum AttestationIdError {
    /// Input string wasn't a valid bech32m encoding at all.
    #[error("{field} bech32 decode failed: {source}")]
    Bech32Decode {
        field: &'static str,
        #[source]
        source: bech32::DecodeError,
    },
    /// Input was bech32m but carried the wrong HRP (typing signal).
    #[error("{field} HRP mismatch: expected `{expected}`, got `{got}`")]
    HrpMismatch {
        field: &'static str,
        expected: &'static str,
        got: String,
    },
    /// Input decoded cleanly but the payload wasn't 32 bytes.
    #[error("{field} length mismatch: expected 32 bytes, got {got}")]
    LengthMismatch { field: &'static str, got: usize },
    /// Encoding the SHA-256 digest as bech32m failed. Should be
    /// unreachable for a 32-byte payload with a valid `lat` HRP, but
    /// propagated rather than unwrapped so the indexer can log
    /// instead of panicking.
    #[error("bech32 encode failed: {0}")]
    Bech32Encode(#[from] bech32::EncodeError),
}

fn decode_hash32(
    s: &str,
    expected_hrp: &'static str,
    field: &'static str,
) -> Result<[u8; 32], AttestationIdError> {
    let (hrp, data) =
        bech32::decode(s).map_err(|source| AttestationIdError::Bech32Decode { field, source })?;
    if hrp.as_str() != expected_hrp {
        return Err(AttestationIdError::HrpMismatch {
            field,
            expected: expected_hrp,
            got: hrp.as_str().to_string(),
        });
    }
    let bytes: [u8; 32] =
        data.as_slice()
            .try_into()
            .map_err(|_| AttestationIdError::LengthMismatch {
                field,
                got: data.len(),
            })?;
    Ok(bytes)
}

/// Derive the canonical `lat1...` AttestationId from its constituent
/// `(schema_id, payload_hash)` pair.
///
/// Both inputs are bech32m strings: `schema_id` must carry the `lsc`
/// HRP and `payload_hash` the `lph` HRP. The result is the 32-byte
/// SHA-256 of the concatenated raw underlying bytes (NOT the bech32m
/// display forms), bech32m-encoded with the `lat` HRP. The function
/// is pure: same input always yields the same id.
pub fn compute_attestation_id(
    schema_id: &str,
    payload_hash: &str,
) -> Result<String, AttestationIdError> {
    let s = decode_hash32(schema_id, HRP_SCHEMA, "schema_id")?;
    let p = decode_hash32(payload_hash, HRP_PAYLOAD, "payload_hash")?;
    let mut hasher = Sha256::new();
    hasher.update(s);
    hasher.update(p);
    let digest: [u8; 32] = hasher.finalize().into();
    let hrp = Hrp::parse(HRP_ATTESTATION).expect("`lat` is a valid hrp");
    Ok(bech32::encode::<Bech32m>(hrp, &digest)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bech32m(hrp_str: &str, data: &[u8]) -> String {
        let hrp = Hrp::parse(hrp_str).unwrap();
        bech32::encode::<Bech32m>(hrp, data).unwrap()
    }

    /// Snapshot vector from
    /// `ligate-chain/crates/modules/attestation/tests/borsh_snapshot.rs`:
    /// `(schema_id = [0x11; 32], payload_hash = [0x33; 32])` SHA-256s
    /// to `b0dcb09af5496e779e60b21109a718475091191efc7a8638b01d51c622fc9128`.
    /// `compute_attestation_id` must produce the bech32m `lat1...`
    /// form of that exact digest, byte-for-byte equal to the chain's
    /// `AttestationId::from_pair(...).to_string()`.
    #[test]
    fn snapshot_matches_chain_reference() {
        let schema_id = bech32m(HRP_SCHEMA, &[0x11u8; 32]);
        let payload_hash = bech32m(HRP_PAYLOAD, &[0x33u8; 32]);

        let id = compute_attestation_id(&schema_id, &payload_hash).unwrap();

        let expected_digest =
            hex::decode("b0dcb09af5496e779e60b21109a718475091191efc7a8638b01d51c622fc9128")
                .unwrap();
        let expected = bech32m(HRP_ATTESTATION, &expected_digest);
        assert_eq!(id, expected);
        assert!(id.starts_with("lat1"));
    }

    #[test]
    fn same_pair_is_deterministic() {
        let schema_id = bech32m(HRP_SCHEMA, &[0x42u8; 32]);
        let payload_hash = bech32m(HRP_PAYLOAD, &[0x99u8; 32]);
        let a = compute_attestation_id(&schema_id, &payload_hash).unwrap();
        let b = compute_attestation_id(&schema_id, &payload_hash).unwrap();
        assert_eq!(a, b);
    }

    /// Concatenation order is part of the contract. Mirrors the
    /// `attestation_id_is_order_sensitive` invariant in the chain's
    /// `unit.rs`.
    #[test]
    fn swapping_inputs_changes_id() {
        let a_as_schema = bech32m(HRP_SCHEMA, &[0xAAu8; 32]);
        let b_as_payload = bech32m(HRP_PAYLOAD, &[0xBBu8; 32]);
        let b_as_schema = bech32m(HRP_SCHEMA, &[0xBBu8; 32]);
        let a_as_payload = bech32m(HRP_PAYLOAD, &[0xAAu8; 32]);
        let id_ab = compute_attestation_id(&a_as_schema, &b_as_payload).unwrap();
        let id_ba = compute_attestation_id(&b_as_schema, &a_as_payload).unwrap();
        assert_ne!(id_ab, id_ba);
    }

    /// HRP is a typing signal. A `lph...` string in the schema_id
    /// slot is a chain-event-shape bug, not a legal input.
    #[test]
    fn wrong_hrp_is_rejected() {
        let bad_schema = bech32m(HRP_PAYLOAD, &[0x11u8; 32]);
        let payload_hash = bech32m(HRP_PAYLOAD, &[0x33u8; 32]);
        let err = compute_attestation_id(&bad_schema, &payload_hash).unwrap_err();
        match err {
            AttestationIdError::HrpMismatch {
                field, expected, ..
            } => {
                assert_eq!(field, "schema_id");
                assert_eq!(expected, HRP_SCHEMA);
            }
            other => panic!("expected HrpMismatch, got {other:?}"),
        }
    }

    #[test]
    fn malformed_bech32_is_rejected() {
        let payload_hash = bech32m(HRP_PAYLOAD, &[0x33u8; 32]);
        let err = compute_attestation_id("not bech32 at all", &payload_hash).unwrap_err();
        assert!(matches!(
            err,
            AttestationIdError::Bech32Decode {
                field: "schema_id",
                ..
            }
        ));
    }
}
