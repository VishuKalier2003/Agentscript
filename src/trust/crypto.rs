// Cryptographic primitives of the trust layer. Security rests on key protection and verification,
// never on hiding the algorithms: SHA-512 (FIPS 180-4) for every full-length digest, with a
// domain-separated, length-prefixed input encoding so no two purposes can share a digest; Ed25519
// (RFC 8032) for record signatures; and the operating system's CSPRNG for identifiers and keys.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha512};

/** Length of a SHA-512 digest in lowercase hexadecimal */
pub(crate) const DIGEST_HEX_LEN: usize = 128;

/** Characters a selection identifier is drawn from */
pub(crate) const ID_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/** Length of a selection identifier */
pub(crate) const ID_LENGTH: usize = 8;

/** Encode bytes as lowercase hexadecimal
 * Input
    - bytes: &[u8] - data to encode
 * Output
    - String of 2 hex characters per byte
*/
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/** Decode lowercase or uppercase hexadecimal into bytes
 * Input
    - text: &str - hex string with an even number of characters
 * Output
    - Result<Vec<u8>, String>
    - Error if the text has an odd length or a non-hex character
*/
pub(crate) fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("hex text has an odd length".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .map_err(|_| "hex text contains a non-hex character".to_string())
        })
        .collect()
}

/** Compute a plain SHA-512 digest of bytes
 * Input
    - bytes: &[u8] - data to digest
 * Output
    - String of 128 lowercase hex characters
*/
pub(crate) fn sha512_hex(bytes: &[u8]) -> String {
    hex(&Sha512::digest(bytes))
}

/** Compute a domain-separated SHA-512 digest, by hashing a fixed prefix, the length-prefixed
 * domain name, the number of parts, and every part prefixed by its length (all lengths as 64-bit
 * big-endian), so different purposes and different part boundaries can never collide
 * Input
    - domain: &str - purpose, such as "selection/content/v1"
    - parts: &[&[u8]] - ordered inputs
 * Output
    - String of 128 lowercase hex characters
*/
pub(crate) fn digest(domain: &str, parts: &[&[u8]]) -> String {
    let mut hasher = Sha512::new();
    hasher.update(b"crane-digest\0");
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((parts.len() as u64).to_be_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hex(&hasher.finalize())
}

/** Serialize JSON canonically: object keys sorted by their UTF-8 bytes at every depth, no
 * insignificant whitespace, and serde_json's standard escaping and number formatting, so the same
 * value always yields the same bytes regardless of map ordering features
 * Input
    - value: &Value - JSON value
 * Output
    - String canonical JSON text
*/
pub(crate) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let fields = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        Value::String(key.clone()),
                        canonical_json(&map[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

/** Compute the domain-separated digest of a JSON value's canonical form
 * Input
    - domain: &str - purpose
    - value: &Value - JSON value
 * Output
    - String of 128 lowercase hex characters
*/
pub(crate) fn digest_json(domain: &str, value: &Value) -> String {
    digest(domain, &[canonical_json(value).as_bytes()])
}

/** Check that text is a full SHA-512 digest: exactly 128 lowercase hex characters
 * Input
    - text: &str - candidate digest
 * Output
    - bool
*/
pub(crate) fn is_digest(text: &str) -> bool {
    text.len() == DIGEST_HEX_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/** Fill a buffer from the operating system's cryptographically secure random generator
 * Input
    - length: usize - number of bytes
 * Output
    - Result<Vec<u8>, String>
    - Error if the operating system generator is unavailable
*/
pub(crate) fn random_bytes(length: usize) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; length];
    getrandom::fill(&mut buffer)
        .map_err(|error| format!("the secure random generator is unavailable: {error}"))?;
    Ok(buffer)
}

/** Generate a random selection identifier of ID_LENGTH characters from ID_ALPHABET, by rejection
 * sampling CSPRNG bytes (bytes at or above the largest multiple of the alphabet size are
 * discarded), so every character is uniformly distributed
 * Input
    - None
 * Output
    - Result<String, String>
    - Error if the random generator is unavailable
*/
pub(crate) fn random_id() -> Result<String, String> {
    let limit = (256 / ID_ALPHABET.len() * ID_ALPHABET.len()) as u8;
    let mut id = String::with_capacity(ID_LENGTH);
    while id.len() < ID_LENGTH {
        for byte in random_bytes(16)? {
            if byte < limit && id.len() < ID_LENGTH {
                id.push(ID_ALPHABET[byte as usize % ID_ALPHABET.len()] as char);
            }
        }
    }
    Ok(id)
}

/** Check that text is a well-formed selection identifier
 * Input
    - text: &str - candidate identifier
 * Output
    - bool, true for exactly ID_LENGTH characters from ID_ALPHABET
*/
pub(crate) fn is_selection_id(text: &str) -> bool {
    text.len() == ID_LENGTH && text.bytes().all(|byte| ID_ALPHABET.contains(&byte))
}

/** An Ed25519 signing key held in memory
 * Fields
    - key: SigningKey - the private key (zeroized on drop by ed25519-dalek)
*/
pub(crate) struct SigningIdentity {
    key: SigningKey,
}

impl SigningIdentity {
    /** Generate a fresh signing key from 32 CSPRNG bytes
     * Input
        - None
     * Output
        - Result<SigningIdentity, String>
        - Error if the random generator is unavailable
    */
    pub(crate) fn generate() -> Result<Self, String> {
        let seed: [u8; 32] = random_bytes(32)?
            .try_into()
            .map_err(|_| "could not generate a key seed".to_string())?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /** Restore a signing key from its 32-byte seed in hex
     * Input
        - seed: &str - 64 hex characters
     * Output
        - Result<SigningIdentity, String>
        - Error if the seed is not 32 bytes of hex
    */
    pub(crate) fn from_seed_hex(seed: &str) -> Result<Self, String> {
        let bytes: [u8; 32] = unhex(seed.trim())?
            .try_into()
            .map_err(|_| "a signing key seed must be 32 bytes".to_string())?;
        Ok(Self {
            key: SigningKey::from_bytes(&bytes),
        })
    }

    /** Return the 32-byte seed in hex, for storing the key in the trusted key directory
     * Input
        - None (uses self)
     * Output
        - String of 64 hex characters
    */
    pub(crate) fn seed_hex(&self) -> String {
        hex(&self.key.to_bytes())
    }

    /** Return the public verifying key in hex
     * Input
        - None (uses self)
     * Output
        - String of 64 hex characters
    */
    pub(crate) fn public_hex(&self) -> String {
        hex(self.key.verifying_key().as_bytes())
    }

    /** Sign a message under a domain, by signing the domain-separated message bytes
     * Input
        - domain: &str - purpose, such as "registry/manifest/v1"
        - message: &[u8] - data to sign
     * Output
        - String of 128 hex characters (the 64-byte signature)
    */
    pub(crate) fn sign(&self, domain: &str, message: &[u8]) -> String {
        hex(&self.key.sign(&signed_message(domain, message)).to_bytes())
    }
}

/** Build the bytes actually signed: a fixed prefix, the domain, a NUL separator, and the message
 * Input
    - domain: &str - purpose
    - message: &[u8] - data
 * Output
    - Vec<u8>
*/
fn signed_message(domain: &str, message: &[u8]) -> Vec<u8> {
    let mut bytes = b"crane-signature\0".to_vec();
    bytes.extend_from_slice(domain.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(message);
    bytes
}

/** Verify an Ed25519 signature made by SigningIdentity::sign, using strict verification (which
 * rejects malleable and small-order encodings)
 * Input
    - public_hex: &str - verifying key in hex
    - domain: &str - purpose the message was signed under
    - message: &[u8] - signed data
    - signature_hex: &str - signature in hex
 * Output
    - Result<(), String>
    - Error if the key or signature is malformed or the signature does not verify
*/
pub(crate) fn verify_signature(
    public_hex: &str,
    domain: &str,
    message: &[u8],
    signature_hex: &str,
) -> Result<(), String> {
    let public: [u8; 32] = unhex(public_hex)?
        .try_into()
        .map_err(|_| "a public key must be 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&public).map_err(|_| "invalid public key".to_string())?;
    let signature: [u8; 64] = unhex(signature_hex)?
        .try_into()
        .map_err(|_| "a signature must be 64 bytes".to_string())?;
    let signature = Signature::from_bytes(&signature);
    key.verify_strict(&signed_message(domain, message), &signature)
        .map_err(|_| "signature verification failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /** Check digest length, domain separation, and part-boundary separation
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn digests_are_full_length_and_domain_separated() {
        let first = digest("a", &[b"xy"]);
        assert!(is_digest(&first));
        assert_eq!(first.len(), 128);
        assert_ne!(first, digest("b", &[b"xy"]));
        assert_ne!(digest("a", &[b"x", b"y"]), digest("a", &[b"xy"]));
        assert_eq!(sha512_hex(b"abc").len(), 128);
        assert!(sha512_hex(b"abc").starts_with("ddaf35a193617aba"));
    }

    /** Check that canonical JSON ignores key order
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn canonical_json_sorts_keys() {
        let left = json!({"b": 1, "a": {"y": [1, 2], "x": "s"}});
        let right = json!({"a": {"x": "s", "y": [1, 2]}, "b": 1});
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_json(&left), r#"{"a":{"x":"s","y":[1,2]},"b":1}"#);
    }

    /** Check identifier shape and that many identifiers do not repeat
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn random_ids_are_well_formed() {
        let ids = (0..500)
            .map(|_| random_id().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 500);
        assert!(ids.iter().all(|id| is_selection_id(id)));
        assert!(!is_selection_id("k7m2p9rx"));
        assert!(!is_selection_id("K7M2P9R"));
    }

    /** Check that signatures verify, and fail for another domain, message, or key
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn signatures_bind_domain_message_and_key() {
        let identity = SigningIdentity::generate().unwrap();
        let other = SigningIdentity::generate().unwrap();
        let signature = identity.sign("d", b"m");
        assert!(verify_signature(&identity.public_hex(), "d", b"m", &signature).is_ok());
        assert!(verify_signature(&identity.public_hex(), "e", b"m", &signature).is_err());
        assert!(verify_signature(&identity.public_hex(), "d", b"n", &signature).is_err());
        assert!(verify_signature(&other.public_hex(), "d", b"m", &signature).is_err());
        let restored = SigningIdentity::from_seed_hex(&identity.seed_hex()).unwrap();
        assert_eq!(restored.public_hex(), identity.public_hex());
    }
}
