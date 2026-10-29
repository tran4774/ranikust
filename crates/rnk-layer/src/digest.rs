//! sha256 digests computed while bytes stream through.

use std::fmt;
use std::io::{self, Write};

// [open: Q-012] sha2 or aws-lc-rs for sha256; sha2 is the interim choice.
use sha2::{Digest, Sha256};

/// A sha256 digest, displayed as `sha256:<64 hex digits>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The 64 lower-case hex digits, without the `sha256:` prefix.
    #[must_use]
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl From<[u8; 32]> for Sha256Digest {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A running sha256 and byte count.
#[derive(Clone, Default)]
pub(crate) struct Hasher {
    sha: Sha256,
    count: u64,
}

impl Hasher {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        // usize is at most 64 bits on every supported target.
        self.count += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    }

    /// The digest and the number of bytes hashed.
    pub(crate) fn finish(self) -> (Sha256Digest, u64) {
        (Sha256Digest(self.sha.finalize().into()), self.count)
    }
}

/// Hashes and counts what is written through it.
pub(crate) struct HashingWriter<W> {
    pub(crate) inner: W,
    pub(crate) hasher: Hasher,
}

impl<W> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Hasher::default(),
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digests() {
        let (empty, n) = Hasher::default().finish();
        assert_eq!(n, 0);
        assert_eq!(
            empty.to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"a").unwrap();
        w.write_all(b"bc").unwrap();
        let (abc, n) = w.hasher.finish();
        assert_eq!(n, 3);
        assert_eq!(
            abc.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(w.inner, b"abc");
    }
}
