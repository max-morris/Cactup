//! Digests for the build cache: SHA-256 (already linked, as rustls' crypto
//! provider and for self-update checksums) over *framed* input.
//!
//! A key is a digest over several things — a compiler's identity, its
//! arguments, a preprocessed source — and each thing over several parts.
//! Concatenating them bare would let two different sets of parts produce
//! the same bytes (`["ab", "c"]` and `["a", "bc"]`); every part therefore
//! goes in behind its length ([`Hasher::feed`], the framing
//! `build::thorn_shapes` uses).

use crate::Res;
use anyhow::Context;
use aws_lc_rs::digest;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub struct Hasher(digest::Context);

impl Hasher {
    /// A hasher for one kind of thing. `domain` says which, so that a
    /// digest of one kind can never be mistaken for another's even over the
    /// same parts.
    pub fn new(domain: &str) -> Self {
        let mut hasher = Self(digest::Context::new(&digest::SHA256));
        hasher.feed(domain.as_bytes());
        hasher
    }

    /// One part, behind its length.
    pub fn feed(&mut self, part: &[u8]) {
        self.0.update(&(part.len() as u64).to_le_bytes());
        self.0.update(part);
    }

    /// Bytes of a part whose length is not known up front (a stream). The
    /// caller frames it: see [`Hasher::end_stream`].
    pub fn stream(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// Close a part fed through [`Hasher::stream`] by its length, after the
    /// fact. Unambiguous as long as a stream is the last thing fed, which
    /// is how the one caller uses it.
    pub fn end_stream(&mut self, len: u64) {
        self.0.update(&len.to_le_bytes());
    }

    pub fn hex(self) -> String {
        hex(self.0.finish().as_ref())
    }
}

/// A plain SHA-256 of a stream of bytes, unframed: the checksum of a store
/// entry (`store`), which is whatever bytes the entry holds.
#[cfg_attr(not(test), allow(dead_code))]
pub struct Checksum(digest::Context);

#[cfg_attr(not(test), allow(dead_code))]
impl Checksum {
    pub fn new() -> Self {
        Self(digest::Context::new(&digest::SHA256))
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn hex(self) -> String {
        hex(self.0.finish().as_ref())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The lowercase hex SHA-256 of `bytes`, unframed: a plain content digest.
pub fn bytes_digest(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

/// The lowercase hex SHA-256 of the file at `path`, unframed.
pub fn file_digest(path: &Path) -> Res<String> {
    let mut file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).with_context(|| format!("Failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        context.update(&buf[..n]);
    }
    Ok(hex(context.finish().as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(domain: &str, parts: &[&str]) -> String {
        let mut hasher = Hasher::new(domain);
        for part in parts {
            hasher.feed(part.as_bytes());
        }
        hasher.hex()
    }

    #[test]
    fn parts_are_framed() {
        assert_ne!(digest_of("k", &["ab", "c"]), digest_of("k", &["a", "bc"]));
        assert_ne!(digest_of("k", &["ab"]), digest_of("k", &["ab", ""]));
        assert_eq!(digest_of("k", &["ab", "c"]), digest_of("k", &["ab", "c"]));
    }

    #[test]
    fn a_domain_separates_kinds() {
        assert_ne!(digest_of("args", &["x"]), digest_of("env", &["x"]));
    }

    #[test]
    fn content_digests_are_plain_sha256() {
        // sha256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(bytes_digest(b"abc"), abc);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("abc");
        std::fs::write(&path, "abc").unwrap();
        assert_eq!(file_digest(&path).unwrap(), abc);
        assert!(file_digest(&tmp.path().join("missing")).is_err());
    }
}
