//! Ed25519 signing and strict verification of update metadata.
//!
//! The message signed is the compact JSON encoding of [`UpdateMetadata`].
//! Struct field order is fixed by declaration order, so signer and verifier
//! agree byte-for-byte without a separate canonicalization layer.

use crate::types::{
    MAX_METADATA_BYTES, MAX_SIGNATURES, SignatureEntry, SignedUpdate, TARGETS_ROLE, UpdateError,
    UpdateMetadata,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Signs a metadata payload with the 32-byte Ed25519 seed and returns the
/// signed update document.
///
/// # Errors
///
/// Returns [`UpdateError::StoreFailed`] only if the fixed-shape payload cannot
/// be JSON encoded. Every 32-byte value is a valid Ed25519 seed.
pub fn sign_update(
    payload: &UpdateMetadata,
    seed: &[u8; 32],
) -> Result<SignedUpdate, UpdateError> {
    let key = SigningKey::from_bytes(seed);
    let canonical = canonical_bytes(payload)?;
    let signature = key.sign(&canonical);
    let entry = SignatureEntry {
        role: payload.role.clone(),
        key_id: hex_encode(&key.verifying_key().to_bytes()),
        signature: hex_encode(&signature.to_bytes()),
    };
    Ok(SignedUpdate {
        payload: payload.clone(),
        signatures: vec![entry],
    })
}

/// Parses a bounded signed metadata document and rejects extension fields.
///
/// # Errors
///
/// Returns [`UpdateError::InvalidMetadata`] for oversized or malformed input.
pub fn parse_signed_update(bytes: &[u8]) -> Result<SignedUpdate, UpdateError> {
    if u64::try_from(bytes.len()).map_err(|_| UpdateError::InvalidMetadata)? > MAX_METADATA_BYTES {
        return Err(UpdateError::InvalidMetadata);
    }
    serde_json::from_slice(bytes).map_err(|_| UpdateError::InvalidMetadata)
}

/// Strictly verifies every signature entry; at least one must validate with
/// the caller-supplied trusted public key.
///
/// The public key is supplied by a trusted caller (the application embeds its
/// publisher root; release tooling receives an independently provisioned
/// verifier key). It is never taken from the metadata document.
///
/// # Errors
///
/// Returns [`UpdateError::InvalidKey`] for a malformed key and
/// [`UpdateError::SignatureInvalid`] when no entry validates.
pub fn verify_signature(
    signed: &SignedUpdate,
    public_key_hex: &str,
) -> Result<(), UpdateError> {
    if signed.payload.role != TARGETS_ROLE
        || signed.signatures.is_empty()
        || signed.signatures.len() > MAX_SIGNATURES
        || signed.signatures.iter().any(|entry| {
            entry.role.len() > 32 || entry.key_id.len() > 64 || entry.signature.len() > 128
        })
    {
        return Err(UpdateError::InvalidMetadata);
    }
    let key_bytes = hex_decode(public_key_hex)?;
    let key_array: [u8; 32] =
        <[u8; 32]>::try_from(key_bytes.as_slice()).map_err(|_| UpdateError::InvalidKey)?;
    let key = VerifyingKey::from_bytes(&key_array).map_err(|_| UpdateError::InvalidKey)?;
    let canonical = canonical_bytes(&signed.payload)?;
    let mut any_valid = false;
    for entry in &signed.signatures {
        if entry.role != signed.payload.role {
            continue;
        }
        if hex_encode(&key.to_bytes()) != entry.key_id {
            continue;
        }
        let signature_bytes = match hex_decode(&entry.signature) {
            Ok(bytes) => bytes,
            Err(UpdateError::InvalidKey) => return Err(UpdateError::SignatureInvalid),
            Err(error) => return Err(error),
        };
        let signature_array: [u8; 64] = <[u8; 64]>::try_from(signature_bytes.as_slice())
            .map_err(|_| UpdateError::SignatureInvalid)?;
        let signature = Signature::from_bytes(&signature_array);
        if key.verify_strict(&canonical, &signature).is_ok() {
            any_valid = true;
        }
    }
    if any_valid {
        Ok(())
    } else {
        Err(UpdateError::SignatureInvalid)
    }
}

/// Derives the key id (lowercase hex of the compressed public key) for a
/// 32-byte Ed25519 seed.
#[must_use]
pub fn public_key_hex(seed: &[u8; 32]) -> String {
    hex_encode(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
}

fn canonical_bytes(payload: &UpdateMetadata) -> Result<Vec<u8>, UpdateError> {
    serde_json::to_vec(payload).map_err(|_| UpdateError::StoreFailed)
}

/// Lowercase hex encoding with a constant alphabet; used for digests, key ids
/// and signatures.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    output
}

/// Lowercase hex decoding with a constant alphabet.
///
/// # Errors
///
/// Returns [`UpdateError::InvalidKey`] for non-hex input or an odd length.
pub fn hex_decode(hex: &str) -> Result<Vec<u8>, UpdateError> {
    if !hex.len().is_multiple_of(2) || hex.chars().any(|character| !character.is_ascii_hexdigit()) {
        return Err(UpdateError::InvalidKey);
    }
    let bytes = hex.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = nibble(pair[0])?;
        let low = nibble(pair[1])?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

const fn nibble(value: u8) -> Result<u8, UpdateError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(UpdateError::InvalidKey),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        hex_decode, hex_encode, parse_signed_update, public_key_hex, sign_update, verify_signature,
    };
    use crate::types::{UpdateError, UpdateMetadata, UpdateTarget};

    const SEED_A: [u8; 32] = [7_u8; 32];
    const SEED_B: [u8; 32] = [9_u8; 32];

    fn payload(version: u64) -> UpdateMetadata {
        UpdateMetadata {
            schema_version: 1,
            role: "targets".to_owned(),
            version,
            expires_at_unix_s: u64::MAX,
            app_version: "0.1.0".to_owned(),
            platform: "x86_64-test".to_owned(),
            install_target: "koe-cli-x86_64-test".to_owned(),
            targets: vec![UpdateTarget {
                path: "koe-cli-x86_64-test".to_owned(),
                sha256: "abc".to_owned(),
                size: 3,
            }],
        }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let signed = sign_update(&payload(1), &SEED_A).expect("sign");
        verify_signature(&signed, &public_key_hex(&SEED_A)).expect("verify");
    }

    #[test]
    fn wrong_key_is_rejected() {
        let signed = sign_update(&payload(1), &SEED_A).expect("sign");
        let error = verify_signature(&signed, &public_key_hex(&SEED_B)).expect_err("wrong key");
        assert_eq!(error, UpdateError::SignatureInvalid);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut signed = sign_update(&payload(1), &SEED_A).expect("sign");
        signed.payload.app_version = "0.2.0".to_owned();
        let error = verify_signature(&signed, &public_key_hex(&SEED_A)).expect_err("tampered");
        assert_eq!(error, UpdateError::SignatureInvalid);
    }

    #[test]
    fn hex_round_trip_is_stable() {
        let bytes = [0xDE_u8, 0xAD, 0xBE, 0xEF, 0x01];
        assert_eq!(hex_encode(&bytes), "deadbeef01");
        assert_eq!(hex_decode("deadbeef01").expect("decode"), bytes);
    }

    #[test]
    fn invalid_hex_is_rejected() {
        assert_eq!(hex_decode("xyz"), Err(UpdateError::InvalidKey));
        assert_eq!(hex_decode("abc"), Err(UpdateError::InvalidKey));
    }

    #[test]
    fn key_id_matches_derived_public_key() {
        let signed = sign_update(&payload(1), &SEED_A).expect("sign");
        assert_eq!(signed.signatures[0].key_id, public_key_hex(&SEED_A));
    }

    #[test]
    fn parser_rejects_unknown_fields_and_oversized_documents() {
        let signed = sign_update(&payload(1), &SEED_A).expect("sign");
        let mut value = serde_json::to_value(signed).expect("value");
        value["payload"]["future_extension"] = serde_json::json!(true);
        let bytes = serde_json::to_vec(&value).expect("json");
        assert_eq!(
            parse_signed_update(&bytes),
            Err(UpdateError::InvalidMetadata)
        );

        let oversized = vec![b' '; usize::try_from(crate::MAX_METADATA_BYTES).expect("size") + 1];
        assert_eq!(
            parse_signed_update(&oversized),
            Err(UpdateError::InvalidMetadata)
        );

        let mut too_many_signatures = sign_update(&payload(1), &SEED_A).expect("sign");
        too_many_signatures.signatures =
            vec![too_many_signatures.signatures[0].clone(); crate::MAX_SIGNATURES + 1];
        assert_eq!(
            verify_signature(&too_many_signatures, &public_key_hex(&SEED_A)),
            Err(UpdateError::InvalidMetadata)
        );
    }
}
