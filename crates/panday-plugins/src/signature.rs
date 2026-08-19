//! `.plugin` archive signing (docs/16 §the package, M16.1).
//!
//! > "Distribution: `.plugin` archive, ed25519-signed; registry tiers
//! > `verified | community | unlisted`".
//!
//! ## What a signature here does and does not mean
//!
//! It proves an archive's bytes are the ones a particular key signed. It says
//! nothing about whether the plugin is *safe* — that is what the capability
//! model and the sandbox tiers are for. Conflating the two is how signed
//! malware gets installed, so `verify_archive` returns a key identity and the
//! caller decides what that identity is worth.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("malformed public key: expected 32 bytes, got {0}")]
    BadPublicKey(usize),
    #[error("malformed signature: expected 64 bytes, got {0}")]
    BadSignature(usize),
    #[error("signature does not match this archive")]
    Mismatch,
    #[error("signature is valid but for a different key than the one trusted")]
    UntrustedKey,
    #[error("hex decode: {0}")]
    Hex(String),
}

/// How much a verified key is trusted (docs/16 §registry tiers).
///
/// Deliberately separate from verification: a signature being *valid* and a
/// signer being *trusted* are different questions, and a type that conflated
/// them would let "the bytes are intact" read as "this is safe to run".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Verified,
    Community,
    Unlisted,
}

/// A key pair, for tests and for the publishing side.
pub struct SigningKeyPair {
    signing: SigningKey,
}

impl SigningKeyPair {
    /// Build from 32 raw bytes.
    ///
    /// No RNG-based constructor: key generation belongs to an operator with a
    /// vetted source of entropy, and offering a convenient `generate()` here
    /// invites someone to create a signing key inside a build script.
    pub fn from_bytes(seed: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }

    pub fn public_key_hex(&self) -> String {
        hex(self.signing.verifying_key().as_bytes())
    }

    /// Sign an archive's content digest.
    pub fn sign_archive(&self, archive: &[u8]) -> String {
        hex(&self.signing.sign(&archive_digest(archive)).to_bytes())
    }
}

/// What is signed: the sha256 of the archive bytes.
///
/// Signing the digest rather than the bytes keeps verification O(1) in
/// signature work for a large archive, and it reuses the same content hash the
/// artifact store uses (docs/03).
fn archive_digest(archive: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(archive);
    h.finalize().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, SignatureError> {
    if !s.len().is_multiple_of(2) {
        return Err(SignatureError::Hex("odd length".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| SignatureError::Hex(e.to_string()))
        })
        .collect()
}

/// Verify an archive against a signature and the public key that made it.
///
/// Returns the verified key's hex identity. Deciding whether *that key* is
/// trusted is the caller's job — see [`Trust`].
pub fn verify_archive(
    archive: &[u8],
    signature_hex: &str,
    public_key_hex: &str,
) -> Result<String, SignatureError> {
    let key_bytes = unhex(public_key_hex)?;
    let key_array: [u8; 32] = key_bytes
        .clone()
        .try_into()
        .map_err(|_| SignatureError::BadPublicKey(key_bytes.len()))?;
    let verifying = VerifyingKey::from_bytes(&key_array)
        .map_err(|_| SignatureError::BadPublicKey(key_array.len()))?;

    let sig_bytes = unhex(signature_hex)?;
    let sig_array: [u8; 64] = sig_bytes
        .clone()
        .try_into()
        .map_err(|_| SignatureError::BadSignature(sig_bytes.len()))?;
    let signature = Signature::from_bytes(&sig_array);

    verifying
        .verify(&archive_digest(archive), &signature)
        .map_err(|_| SignatureError::Mismatch)?;

    Ok(hex(verifying.as_bytes()))
}

/// Verify, and require the signer to be a specific expected key.
///
/// The form a registry client should use: verifying a signature against
/// whatever key shipped *with* the archive proves only that the archive is
/// internally consistent, which an attacker can arrange trivially by signing
/// their own payload with their own key.
pub fn verify_from_trusted_key(
    archive: &[u8],
    signature_hex: &str,
    trusted_public_key_hex: &str,
) -> Result<(), SignatureError> {
    let signer = verify_archive(archive, signature_hex, trusted_public_key_hex)?;
    if signer.eq_ignore_ascii_case(trusted_public_key_hex) {
        Ok(())
    } else {
        Err(SignatureError::UntrustedKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair(seed: u8) -> SigningKeyPair {
        SigningKeyPair::from_bytes(&[seed; 32])
    }

    #[test]
    fn a_signature_verifies_against_its_own_archive() {
        let k = keypair(1);
        let archive = b"a plugin archive";
        let sig = k.sign_archive(archive);
        let signer = verify_archive(archive, &sig, &k.public_key_hex()).unwrap();
        assert_eq!(signer, k.public_key_hex());
    }

    #[test]
    fn one_flipped_byte_fails_verification() {
        // The property the whole mechanism exists for.
        let k = keypair(1);
        let sig = k.sign_archive(b"original content");
        let err = verify_archive(b"originaP content", &sig, &k.public_key_hex()).unwrap_err();
        assert!(matches!(err, SignatureError::Mismatch), "{err}");
    }

    #[test]
    fn a_signature_from_another_key_does_not_verify() {
        let mine = keypair(1);
        let theirs = keypair(2);
        let archive = b"payload";
        let sig = theirs.sign_archive(archive);

        let err = verify_archive(archive, &sig, &mine.public_key_hex()).unwrap_err();
        assert!(matches!(err, SignatureError::Mismatch));
    }

    #[test]
    fn verifying_against_a_bundled_key_is_not_enough_which_is_why_trusted_keys_exist() {
        // An attacker signs their own payload with their own key. The signature
        // is perfectly valid; the key is not one we trust.
        let attacker = keypair(9);
        let archive = b"malicious payload";
        let sig = attacker.sign_archive(archive);

        // Self-consistent, and therefore useless as a safety check on its own.
        assert!(verify_archive(archive, &sig, &attacker.public_key_hex()).is_ok());

        // Against a key we actually trust, it fails.
        let trusted = keypair(1);
        assert!(matches!(
            verify_from_trusted_key(archive, &sig, &trusted.public_key_hex()),
            Err(SignatureError::Mismatch)
        ));
    }

    #[test]
    fn malformed_inputs_are_rejected_with_a_reason() {
        let k = keypair(1);
        let archive = b"x";
        let sig = k.sign_archive(archive);

        assert!(matches!(
            verify_archive(archive, &sig, "abcd"),
            Err(SignatureError::BadPublicKey(_))
        ));
        assert!(matches!(
            verify_archive(archive, "beef", &k.public_key_hex()),
            Err(SignatureError::BadSignature(_))
        ));
        assert!(matches!(
            verify_archive(archive, "zz", &k.public_key_hex()),
            Err(SignatureError::Hex(_))
        ));
    }

    #[test]
    fn an_empty_archive_still_signs_and_verifies() {
        // Degenerate but legal; failing here would be a surprise at publish time.
        let k = keypair(3);
        let sig = k.sign_archive(b"");
        assert!(verify_archive(b"", &sig, &k.public_key_hex()).is_ok());
    }

    #[test]
    fn trust_is_a_separate_question_from_validity() {
        // Documented by type: nothing in this module returns `Trust`, because a
        // valid signature does not imply a trusted signer.
        assert_ne!(Trust::Verified, Trust::Community);
        assert_ne!(Trust::Community, Trust::Unlisted);
    }
}
