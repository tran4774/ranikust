//! Content digests, `<algorithm>:<encoded>`, as OCI descriptors write them.
//!
//! Only the two registered algorithms whose encoding is lowercase hex are
//! accepted: `sha256` (what ranikust writes) and `sha512` (accepted when
//! read). See the OCI image-spec v1.1.1
//! [descriptor.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md#digests).

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, excerpt};

/// A hash algorithm a [`Digest`] may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DigestAlgorithm {
    /// SHA-256: 64 lowercase hex characters. The algorithm ranikust writes.
    Sha256,
    /// SHA-512: 128 lowercase hex characters. Accepted when read.
    Sha512,
}

impl DigestAlgorithm {
    /// The algorithm's name in a digest string, such as `sha256`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Length of the encoded (hex) part, in characters.
    pub const fn encoded_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "sha256" => Some(Self::Sha256),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

impl fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated content digest, such as `sha256:e3b0c442…b855`.
///
/// Parsing accepts exactly `sha256:` followed by 64, or `sha512:` followed by
/// 128, lowercase hex characters; anything else is an error, never a panic.
/// Values order like their strings. Serialized as the plain string.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algorithm: DigestAlgorithm,
    /// The whole digest, `<algorithm>:<encoded>`.
    value: String,
}

impl Digest {
    /// Parses and validates a digest string.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let algorithm = validate(input).map_err(|reason| Error::InvalidDigest {
            input: excerpt(input),
            reason,
        })?;
        Ok(Self {
            algorithm,
            value: input.to_owned(),
        })
    }

    /// Builds a digest from an algorithm and its encoded (hex) part.
    pub fn from_parts(algorithm: DigestAlgorithm, encoded: &str) -> Result<Self, Error> {
        Self::parse(&format!("{algorithm}:{encoded}"))
    }

    /// The digest of a SHA-256 hash output.
    pub fn from_sha256_bytes(hash: &[u8; 32]) -> Self {
        Self::from_hash_bytes(DigestAlgorithm::Sha256, hash)
    }

    /// The digest of a SHA-512 hash output.
    pub fn from_sha512_bytes(hash: &[u8; 64]) -> Self {
        Self::from_hash_bytes(DigestAlgorithm::Sha512, hash)
    }

    fn from_hash_bytes(algorithm: DigestAlgorithm, hash: &[u8]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let name = algorithm.as_str();
        let mut value = String::with_capacity(name.len() + 1 + 2 * hash.len());
        value.push_str(name);
        value.push(':');
        for byte in hash {
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self { algorithm, value }
    }

    /// The hash algorithm.
    pub fn algorithm(&self) -> DigestAlgorithm {
        self.algorithm
    }

    /// The encoded part: the lowercase hex after the `:`.
    pub fn encoded(&self) -> &str {
        self.value
            .split_once(':')
            .map_or("", |(_, encoded)| encoded)
    }

    /// The whole digest, `<algorithm>:<encoded>`.
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

/// Checks `input` and returns its algorithm, or why it is rejected.
fn validate(input: &str) -> Result<DigestAlgorithm, &'static str> {
    let (name, encoded) = input.split_once(':').ok_or("expected <algorithm>:<hex>")?;
    let algorithm = DigestAlgorithm::from_name(name)
        .ok_or("unsupported algorithm, expected sha256 or sha512")?;
    if encoded.len() != algorithm.encoded_len() {
        return Err(match algorithm {
            DigestAlgorithm::Sha256 => "a sha256 digest has 64 hex characters",
            DigestAlgorithm::Sha512 => "a sha512 digest has 128 hex characters",
        });
    }
    if !encoded
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("the encoded part must be lowercase hex (0-9, a-f)");
    }
    Ok(algorithm)
}

impl Ord for Digest {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl PartialOrd for Digest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Digest").field(&self.value).finish()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl FromStr for Digest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256 of the empty input.
    const EMPTY_SHA256: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    /// sha512 of the empty input.
    const EMPTY_SHA512: &str = "sha512:cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";

    fn hex_bytes<const N: usize>(hex: &str) -> [u8; N] {
        let mut out = [0u8; N];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn parses_sha256_and_sha512() {
        let d = Digest::parse(EMPTY_SHA256).unwrap();
        assert_eq!(d.algorithm(), DigestAlgorithm::Sha256);
        assert_eq!(d.encoded().len(), 64);
        assert_eq!(d.as_str(), EMPTY_SHA256);
        assert_eq!(d.to_string(), EMPTY_SHA256);

        let d: Digest = EMPTY_SHA512.parse().unwrap();
        assert_eq!(d.algorithm(), DigestAlgorithm::Sha512);
        assert_eq!(d.encoded().len(), 128);
    }

    #[test]
    fn from_hash_bytes_writes_lowercase_hex() {
        let encoded = EMPTY_SHA256.strip_prefix("sha256:").unwrap();
        let d = Digest::from_sha256_bytes(&hex_bytes::<32>(encoded));
        assert_eq!(d, Digest::parse(EMPTY_SHA256).unwrap());

        let encoded = EMPTY_SHA512.strip_prefix("sha512:").unwrap();
        let d = Digest::from_sha512_bytes(&hex_bytes::<64>(encoded));
        assert_eq!(d.as_str(), EMPTY_SHA512);

        let d = Digest::from_sha256_bytes(&[0xff; 32]);
        assert_eq!(d.encoded(), "f".repeat(64));
    }

    #[test]
    fn from_parts_validates() {
        let encoded = "a".repeat(64);
        let d = Digest::from_parts(DigestAlgorithm::Sha256, &encoded).unwrap();
        assert_eq!(d.as_str(), format!("sha256:{encoded}"));
        assert!(Digest::from_parts(DigestAlgorithm::Sha512, &encoded).is_err());
    }

    #[test]
    fn rejects_hostile_strings() {
        let hex64 = "0123456789abcdef".repeat(4);
        let cases = [
            String::new(),
            ":".to_owned(),
            "sha256".to_owned(),
            "sha256:".to_owned(),
            hex64.clone(),
            format!(":{hex64}"),
            format!("SHA256:{hex64}"),
            format!("sha384:{}", "a".repeat(96)),
            format!("md5:{}", "a".repeat(32)),
            format!("sha256+b64u:{hex64}"),
            format!("sha256:{}", &hex64[1..]),
            format!("sha256:{hex64}0"),
            format!("sha256:{}", hex64.to_uppercase()),
            format!("sha256:{}g", &hex64[1..]),
            format!("sha256: {}", &hex64[1..]),
            format!(" sha256:{hex64}"),
            format!("sha256:{hex64} "),
            format!("sha256:{hex64}\n"),
            format!("sha256:{}\0", &hex64[1..]),
            format!("sha256:{hex64}:{hex64}"),
            format!("sha256:{}é", &hex64[2..]),
            format!("sha512:{hex64}"),
            "sha256:".to_owned() + &"é".repeat(32),
            "x".repeat(1 << 16),
        ];
        for case in &cases {
            let err = Digest::parse(case).unwrap_err();
            assert!(
                matches!(err, Error::InvalidDigest { .. }),
                "{case:?} gave {err:?}"
            );
            // Messages quote only an excerpt of the input.
            assert!(err.to_string().len() < 300, "{case:?}");
        }
    }

    #[test]
    fn orders_like_strings() {
        let a = Digest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
        let b = Digest::parse(&format!("sha256:{}", "1".repeat(64))).unwrap();
        let c = Digest::parse(&format!("sha512:{}", "0".repeat(128))).unwrap();
        let mut sorted = vec![c.clone(), b.clone(), a.clone()];
        sorted.sort();
        assert_eq!(sorted, [a, b, c]);
    }

    #[test]
    fn serde_round_trip() {
        let d = Digest::parse(EMPTY_SHA256).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{EMPTY_SHA256}\""));
        let back: Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);

        assert!(serde_json::from_str::<Digest>("\"sha256:abc\"").is_err());
        assert!(serde_json::from_str::<Digest>("42").is_err());
        assert!(serde_json::from_str::<Digest>("null").is_err());
    }
}
