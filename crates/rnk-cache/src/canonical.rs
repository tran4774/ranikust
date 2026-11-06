//! The byte form a key or a tag hashes.

use sha2::{Digest as _, Sha256};

/// SHA-256 fed with self-delimiting fields: a variable-length field carries
/// its length, a list its count and an optional field a presence byte, all
/// big-endian, so two different field sequences never feed the same bytes.
pub(crate) struct Canonical {
    sha: Sha256,
}

impl Canonical {
    /// Starts a hash whose first field names what it hashes, so a key of
    /// one kind never equals one of another kind.
    pub(crate) fn new(domain: &str) -> Self {
        let mut c = Self { sha: Sha256::new() };
        c.str(domain);
        c
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.sha.update([value]);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.sha.update(value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.sha.update(value.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.sha.update(value.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.sha.update(value.to_be_bytes());
    }

    pub(crate) fn hash(&mut self, hash: &[u8; 32]) {
        self.sha.update(hash);
    }

    /// The count of a list, or the length of a field.
    pub(crate) fn len(&mut self, len: usize) {
        // usize is at most 64 bits on every supported target.
        self.u64(u64::try_from(len).unwrap_or(u64::MAX));
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.sha.update(bytes);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub(crate) fn opt_bytes(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            None => self.u8(0),
            Some(bytes) => {
                self.u8(1);
                self.bytes(bytes);
            }
        }
    }

    pub(crate) fn opt_str(&mut self, s: Option<&str>) {
        self.opt_bytes(s.map(str::as_bytes));
    }

    pub(crate) fn strs<S: AsRef<str>>(&mut self, list: &[S]) {
        self.len(list.len());
        for s in list {
            self.str(s.as_ref());
        }
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        self.sha.finalize().into()
    }
}
