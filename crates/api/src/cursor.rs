//! Opaque base64url-encoded pagination cursors, per RFC 0001.
//!
//! Each list endpoint defines its own cursor JSON shape (e.g.
//! `{"slot": 12345}` for `/v1/blocks`, `{"slot": 12345, "idx": 7}`
//! for `/v1/txs`). The encoder serialises that shape to JSON,
//! base64url-encodes the bytes, and the decoder reverses it.
//!
//! Clients **MUST** treat the encoded string as opaque. Server-side
//! we're free to change the internal JSON shape (add a tiebreaker,
//! switch from slot-based to a global tx index) without breaking
//! clients — they just round-trip whatever they last received.
//!
//! Why base64url-encoded JSON instead of, say, raw `slot=12345`:
//!
//! - Same shape works for compound keys (`{"slot": 12345, "idx": 7}`)
//!   without inventing a delimiter and worrying about escapes.
//! - URL-safe alphabet means cursors survive being threaded through
//!   any URL builder without percent-encoding gymnastics.
//! - Easy to evolve: adding a field to the JSON shape doesn't break
//!   old cursors that happened to be in the wild — decode just sees
//!   an extra field it ignores.
//!
//! ## Default + max page size
//!
//! Per RFC 0001:
//!
//! - Default `limit` when client omits the query param: 20.
//! - Server clamps to `MAX_LIMIT = 100` silently (no error). A
//!   client asking for 500 gets 100 and a `next` cursor; another
//!   request advances them. Clamping silently is a deliberate UX
//!   call: error responses for over-limit requests are noise, and
//!   no partner has a reason to set `limit=10000` for a real query.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{de::DeserializeOwned, Serialize};

/// Default `limit` for list endpoints when the client omits `?limit=`.
pub const DEFAULT_LIMIT: u32 = 20;

/// Server-side ceiling for `?limit=`. Clamped silently when the
/// client asks for more (RFC 0001 §"Request shape").
pub const MAX_LIMIT: u32 = 100;

/// Clamp a caller-supplied `limit` to `[1, MAX_LIMIT]`, with `None`
/// falling back to [`DEFAULT_LIMIT`]. `0` collapses to the default
/// (a `limit=0` query would otherwise return an empty page forever).
pub fn resolve_limit(requested: Option<u32>) -> u32 {
    match requested {
        Some(0) | None => DEFAULT_LIMIT,
        Some(n) => n.min(MAX_LIMIT),
    }
}

/// Encode a cursor shape `T` as a base64url-no-pad string.
///
/// Returns the encoded cursor. Infallible for any cursor `T` that
/// serializes successfully (which is every shape we ship — they're
/// all simple struct-of-scalars; serde_json::to_vec only fails on
/// trait-object or RefCell-cycle situations).
pub fn encode<T: Serialize>(cursor: &T) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(cursor)
        .map_err(|e| anyhow::anyhow!("encoding cursor shape to JSON: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Decode a base64url-no-pad cursor string back into shape `T`.
///
/// Returns `None` on every error path (bad base64, bad JSON, wrong
/// shape). The endpoint treats "couldn't decode cursor" as "start
/// at the head" rather than 400-ing — opaque cursors mean clients
/// can't reasonably be expected to debug them, and the page they
/// get is still correct (just from the head, not from where they
/// thought they were).
pub fn decode<T: DeserializeOwned>(cursor: &str) -> Option<T> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct BlocksCursor {
        slot: u64,
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TxsCursor {
        slot: u64,
        idx: u32,
    }

    #[test]
    fn resolve_limit_defaults_when_unset() {
        assert_eq!(resolve_limit(None), DEFAULT_LIMIT);
    }

    #[test]
    fn resolve_limit_defaults_on_zero() {
        // Zero is treated like None — a literal `limit=0` request
        // would otherwise wedge the client (empty pages forever).
        assert_eq!(resolve_limit(Some(0)), DEFAULT_LIMIT);
    }

    #[test]
    fn resolve_limit_clamps_to_max() {
        assert_eq!(resolve_limit(Some(MAX_LIMIT + 1)), MAX_LIMIT);
        assert_eq!(resolve_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn resolve_limit_passes_through_in_range() {
        assert_eq!(resolve_limit(Some(1)), 1);
        assert_eq!(resolve_limit(Some(50)), 50);
        assert_eq!(resolve_limit(Some(MAX_LIMIT)), MAX_LIMIT);
    }

    #[test]
    fn blocks_cursor_roundtrip() {
        let original = BlocksCursor { slot: 12_345 };
        let encoded = encode(&original).unwrap();
        let decoded: BlocksCursor = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn compound_cursor_roundtrip() {
        let original = TxsCursor {
            slot: 9_876_543,
            idx: 42,
        };
        let encoded = encode(&original).unwrap();
        let decoded: TxsCursor = decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn encoded_cursor_is_url_safe() {
        // Picking a slot that produces `+` / `/` chars in standard
        // base64 to confirm we're on the URL-safe alphabet.
        let cursor = BlocksCursor { slot: u64::MAX };
        let encoded = encode(&cursor).unwrap();
        assert!(!encoded.contains('+'), "got non-URL-safe char: {encoded}");
        assert!(!encoded.contains('/'), "got non-URL-safe char: {encoded}");
        assert!(!encoded.contains('='), "no padding expected: {encoded}");
    }

    #[test]
    fn decode_rejects_garbage_returns_none() {
        // Bad base64 -> None
        assert!(decode::<BlocksCursor>("!!! not base64 !!!").is_none());
        // Valid base64 but not JSON for the target shape -> None
        assert!(decode::<BlocksCursor>("aGVsbG8").is_none()); // "hello"
                                                              // Valid JSON but wrong shape -> None
        let wrong_shape = URL_SAFE_NO_PAD.encode(br#"{"other": 1}"#);
        assert!(decode::<BlocksCursor>(&wrong_shape).is_none());
    }
}
