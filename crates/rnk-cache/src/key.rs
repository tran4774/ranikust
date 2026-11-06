//! The chained cache key (ADR-0005, ADR-0113) and the tag its entry is
//! looked up by.

use std::fmt;

use rnk_oci::{Compression, Digest, Platform};

use crate::canonical::Canonical;
use crate::inputs::KeyInputs;

/// The version of the key algorithm, hashed into every key and named in
/// every tag. Any change to what a key or a tag hashes, or how, bumps it,
/// and so does any change to the entry form a lookup checks or to the
/// layer format, what a cached layer holds for given inputs: [`Encoding`]
/// names no encoder version. New keys then miss the old entries, which are
/// never rewritten (ADR-0005).
pub const KEY_VERSION: u32 = 2;

/// A step's cache key: `key(k) = SHA-256(version, key(k-1), instruction,
/// inputs(k))`, chained from [`CacheKey::root`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    version: u32,
    hash: [u8; 32],
}

impl CacheKey {
    /// The chain's root: the base image's platform manifest digest (`None`
    /// for `FROM scratch`) and the platform the stage is built for.
    #[must_use]
    pub fn root(base: Option<&Digest>, platform: &Platform) -> Self {
        // Destructured so that a field added to `Platform` cannot be left out.
        let Platform {
            architecture,
            os,
            os_version,
            os_features,
            variant,
            features,
        } = platform;
        let mut c = Canonical::new("rnk-cache root");
        c.u32(KEY_VERSION);
        c.opt_str(base.map(Digest::as_str));
        c.str(architecture);
        c.str(os);
        c.opt_str(os_version.as_deref());
        match os_features {
            None => c.u8(0),
            Some(features) => {
                c.u8(1);
                c.strs(features);
            }
        }
        c.opt_str(variant.as_deref());
        match features {
            None => c.u8(0),
            Some(features) => {
                c.u8(1);
                c.strs(features);
            }
        }
        Self {
            version: KEY_VERSION,
            hash: c.finish(),
        }
    }

    /// The key of the step after `self`: `instruction` is the step's text
    /// after variable expansion, with its flags. The key names no encoding:
    /// [`next_entry`](Self::next_entry) gives the step's entry.
    #[must_use]
    pub fn next(&self, instruction: &str, inputs: KeyInputs) -> Self {
        let mut c = Canonical::new("rnk-cache step");
        c.u32(KEY_VERSION);
        c.u32(self.version);
        c.hash(&self.hash);
        c.str(instruction);
        inputs.feed(&mut c);
        Self {
            version: KEY_VERSION,
            hash: c.finish(),
        }
    }

    /// The entry of the step after `self`: the key [`next`](Self::next)
    /// gives, with the encoding `inputs` were made for.
    #[must_use]
    pub fn next_entry(&self, instruction: &str, inputs: KeyInputs) -> EntryId {
        let encoding = inputs.encoding();
        EntryId {
            key: self.next(instruction, inputs),
            encoding,
        }
    }

    /// The version of the algorithm that made the key.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The key's SHA-256.
    #[must_use]
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = Digest::from_sha256_bytes(&self.hash);
        write!(f, "CacheKey(v{}:{})", self.version, hex.encoded())
    }
}

/// What a cache entry is looked up and pushed by: a step's key and the
/// encoding of its layer, together, so that the tag, the annotations a
/// lookup checks and the layer it accepts all name one encoding
/// ([ADR-0113]).
///
/// Only [`CacheKey::next_entry`] makes one, with the encoding the key's
/// inputs were made for: a key that left mtimes out never names an entry
/// of `--reproducible` layers, nor the reverse.
///
/// [ADR-0113]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntryId {
    key: CacheKey,
    encoding: Encoding,
}

impl EntryId {
    /// The step's key.
    #[must_use]
    pub fn key(&self) -> &CacheKey {
        &self.key
    }

    /// The layer's encoding.
    #[must_use]
    pub fn encoding(&self) -> &Encoding {
        &self.encoding
    }

    /// The tag the entry is looked up and pushed under:
    /// `rnk-v<version>-<hex>`, where `<hex>` is the SHA-256 of the key and
    /// the encoding, so a hit was encoded with this build's settings.
    #[must_use]
    pub fn tag(&self) -> String {
        let Self { key, encoding } = self;
        let Encoding {
            compression,
            level,
            times,
        } = *encoding;
        let mut c = Canonical::new("rnk-cache tag");
        c.u32(key.version);
        c.hash(&key.hash);
        match compression {
            // The encoder ignores the level of a plain tar.
            Compression::Uncompressed => c.str("uncompressed"),
            Compression::Gzip => {
                c.str("gzip");
                c.i32(level);
            }
            Compression::Zstd => {
                c.str("zstd");
                c.i32(level);
            }
        }
        match times {
            TimePolicy::Kept => c.str("kept"),
            TimePolicy::Clamped { epoch } => {
                c.str("clamped");
                c.i64(epoch);
            }
        }
        let hex = Digest::from_sha256_bytes(&c.finish());
        format!("rnk-v{}-{}", key.version, hex.encoded())
    }
}

/// How a step's layer is encoded: whatever besides the key the layer's
/// bytes depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Encoding {
    /// The blob's compression.
    pub compression: Compression,
    /// The compression level, as the encoder takes it; ignored when
    /// uncompressed.
    pub level: i32,
    /// The mtimes in the layer's headers.
    pub times: TimePolicy,
}

/// The form a cache entry's annotation names the encoding in:
/// `gzip;level=6;times=kept`, `zstd;level=3;times=clamped;epoch=0`,
/// `uncompressed;times=kept` (no level, which a plain tar ignores).
impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.compression {
            Compression::Uncompressed => f.write_str("uncompressed")?,
            Compression::Gzip => write!(f, "gzip;level={}", self.level)?,
            Compression::Zstd => write!(f, "zstd;level={}", self.level)?,
        }
        match self.times {
            TimePolicy::Kept => f.write_str(";times=kept"),
            TimePolicy::Clamped { epoch } => write!(f, ";times=clamped;epoch={epoch}"),
        }
    }
}

/// The mtimes a layer's headers carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimePolicy {
    /// Each file keeps its own: a build without `--reproducible`.
    Kept,
    /// `--reproducible`: an mtime later than `epoch`, in seconds since the
    /// Unix epoch, is clamped to it.
    Clamped {
        /// `SOURCE_DATE_EPOCH`, or 0.
        epoch: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag of a key paired with any encoding, which only this module
    /// can build: `golden.rs` reaches tags through `next_entry`, whose key
    /// already depends on the time policy, so this pins the tag hash
    /// alone. A change here bumps `KEY_VERSION`.
    #[test]
    fn the_tag_hash_is_pinned_for_any_key_and_encoding() {
        // `golden.rs`'s "step with every input" key, made with kept times.
        let hex = "a949f4ffdf8c45cdd972d636face070a22bc8e7090b6d40f0cf971a06fdee1bf";
        let mut hash = [0_u8; 32];
        for (byte, pair) in hash.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        let key = CacheKey {
            version: KEY_VERSION,
            hash,
        };
        let tag = |compression, level, times| {
            let encoding = Encoding {
                compression,
                level,
                times,
            };
            EntryId { key, encoding }.tag()
        };
        assert_eq!(
            [
                tag(Compression::Gzip, 6, TimePolicy::Kept),
                tag(Compression::Zstd, 3, TimePolicy::Clamped { epoch: 0 }),
                tag(
                    Compression::Uncompressed,
                    0,
                    TimePolicy::Clamped {
                        epoch: 1_700_000_000,
                    },
                ),
            ],
            [
                "rnk-v2-94a5f7718b77018ac1be744ed70692af43c77d2716dcf501d6615008216e38a8",
                "rnk-v2-4c0be455b28f53568ffe1ff1292acf63d1ae87fb3af1ef0bb8524cbabe788193",
                "rnk-v2-fab985be670211bd2a2ace5aaf692beec32ffdcc269d9624bf0797ef9715cfc9",
            ],
            "the tag hash changed: bump KEY_VERSION (now {KEY_VERSION}) and update \
             every golden"
        );
    }
}
