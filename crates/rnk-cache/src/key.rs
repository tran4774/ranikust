//! The chained cache key (ADR-0005, ADR-0113) and the tag its entry is
//! looked up by.

use std::fmt;

use rnk_oci::{Compression, Digest, Platform};

use crate::canonical::Canonical;
use crate::inputs::KeyInputs;

/// The version of the key algorithm, hashed into every key and named in
/// every tag. Any change to what a key or a tag hashes, or how, bumps it:
/// new keys then miss the old entries, which are never rewritten
/// (ADR-0005).
pub const KEY_VERSION: u32 = 1;

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
        let mut c = Canonical::new("rnk-cache root");
        c.u32(KEY_VERSION);
        c.opt_str(base.map(Digest::as_str));
        c.str(&platform.architecture);
        c.str(&platform.os);
        c.opt_str(platform.os_version.as_deref());
        match &platform.os_features {
            None => c.u8(0),
            Some(features) => {
                c.u8(1);
                c.strs(features);
            }
        }
        c.opt_str(platform.variant.as_deref());
        match &platform.features {
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
    /// after variable expansion, with its flags.
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

    /// The tag this step's layer is looked up and pushed under when encoded
    /// as `encoding`: `rnk-v<version>-<hex>`, where `<hex>` is the SHA-256
    /// of the key and the encoding, so a hit has the bytes this build would
    /// write (ADR-0113).
    #[must_use]
    pub fn tag(&self, encoding: &Encoding) -> String {
        let mut c = Canonical::new("rnk-cache tag");
        c.u32(self.version);
        c.hash(&self.hash);
        c.str(match encoding.compression {
            Compression::Uncompressed => "uncompressed",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        });
        c.i32(encoding.level);
        match encoding.times {
            TimePolicy::Kept => c.str("kept"),
            TimePolicy::Clamped { epoch } => {
                c.str("clamped");
                c.i64(epoch);
            }
        }
        let hex = Digest::from_sha256_bytes(&c.finish());
        format!("rnk-v{}-{}", self.version, hex.encoded())
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = Digest::from_sha256_bytes(&self.hash);
        write!(f, "CacheKey(v{}:{})", self.version, hex.encoded())
    }
}

/// How a step's layer is encoded: whatever besides the key the layer's
/// bytes depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Encoding {
    /// The blob's compression.
    pub compression: Compression,
    /// The compression level, as the encoder takes it.
    pub level: i32,
    /// The mtimes in the layer's headers.
    pub times: TimePolicy,
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
