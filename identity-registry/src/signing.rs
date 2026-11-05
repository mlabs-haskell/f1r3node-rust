// secp256k1 signing and verification over the payload digest. Matches the
// scheme that rholang/identity_registry.rho uses on-chain via
// rho:crypto:secp256k1Verify.

use crate::payload::blake2b256;
use secp256k1::ecdsa::Signature;
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};

/// Signs `blake2b256(payload)` under `sk` and returns the ECDSA signature.
pub fn sign_payload(payload: &[u8], sk: &SecretKey) -> Signature {
    let digest = blake2b256(payload);
    let message = Message::from_digest(digest);
    Secp256k1::new().sign_ecdsa(message, sk)
}

/// Verifies that `sig` is a valid signature over `blake2b256(payload)` under
/// `pk`. Used by off-chain tests. The on-chain contract performs its own
/// verification via `rho:crypto:secp256k1Verify`.
pub fn verify_payload(
    payload: &[u8],
    sig: &Signature,
    pk: &PublicKey,
) -> Result<(), secp256k1::Error> {
    let digest = blake2b256(payload);
    let message = Message::from_digest(digest);
    Secp256k1::new().verify_ecdsa(message, sig, pk)
}

/// Serialises a public key into its 65-byte uncompressed form, matching the
/// shape the Rholang contract expects on the wire.
pub fn uncompressed_pk_bytes(pk: &PublicKey) -> [u8; 65] {
    pk.serialize_uncompressed()
}

/// Serialises a signature in DER form, matching the format
/// `rho:crypto:secp256k1Verify` consumes.
pub fn signature_der(sig: &Signature) -> Vec<u8> {
    sig.serialize_der().to_vec()
}
