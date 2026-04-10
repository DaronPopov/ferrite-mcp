//! Content hashing for the determinism contract.
//!
//! Every file-level compare-and-swap in the mutation layer goes through
//! `content_hash`. The algorithm is blake3, but the public API uses the
//! name `content_hash` so the hashing algorithm can be swapped in the
//! future without breaking any tool schemas.
//!
//! - `hash_bytes`: hash an in-memory slice.
//! - `hash_file`:  stream-hash a file on disk without loading it whole.
//! - `hash_reader`: stream-hash any `Read` source.
//!
//! All outputs are lowercase hex, produced by blake3's canonical
//! `to_hex().to_string()` — never reconstructed by hand.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Buffer size used for streaming hashes. 64 KiB is a good trade-off
/// between syscall count and cache footprint for modern hardware.
const HASH_BUF: usize = 64 * 1024;

/// Hash an in-memory byte slice. Fast path — no allocations beyond the
/// output hex string.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Stream-hash any `Read` source without materialising the content in
/// memory. Uses blake3's incremental hasher with a fixed 64 KiB buffer.
pub fn hash_reader<R: Read>(mut reader: R) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; HASH_BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Stream-hash a file on disk. Opens, hashes, closes — no mmap, no
/// whole-file load. Works on files larger than available RAM.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    hash_reader(file)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// blake3 of the empty input is a well-known fixed value; lock it in.
    const EMPTY_BLAKE3: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn hash_bytes_empty() {
        assert_eq!(hash_bytes(&[]), EMPTY_BLAKE3);
    }

    #[test]
    fn hash_bytes_known() {
        // blake3("hello world") — pinned to detect accidental algorithm swaps.
        let h = hash_bytes(b"hello world");
        assert_eq!(
            h,
            "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24"
        );
    }

    #[test]
    fn hash_bytes_and_reader_agree() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let a = hash_bytes(data);
        let b = hash_reader(&data[..]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn hash_file_empty() {
        let f = NamedTempFile::new().unwrap();
        assert_eq!(hash_file(f.path()).unwrap(), EMPTY_BLAKE3);
    }

    #[test]
    fn hash_file_small() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello world").unwrap();
        f.flush().unwrap();
        let h = hash_file(f.path()).unwrap();
        assert_eq!(h, hash_bytes(b"hello world"));
    }

    /// Streams a file that is strictly larger than `HASH_BUF` to exercise
    /// the multi-chunk path of the incremental hasher. The expected hash
    /// is computed against the same bytes held in memory via `hash_bytes`
    /// — so the test verifies that chunking matches the one-shot API.
    #[test]
    fn hash_file_larger_than_buffer() {
        // 200 KiB — forces at least 3 read iterations through HASH_BUF.
        let payload: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&payload).unwrap();
        f.flush().unwrap();

        let from_file = hash_file(f.path()).unwrap();
        let from_mem = hash_bytes(&payload);
        assert_eq!(from_file, from_mem);
    }

    #[test]
    fn hash_file_not_found_returns_io_error() {
        let err = hash_file(Path::new("/nonexistent/ferrite/hash/path")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn hash_is_stable_across_calls() {
        let data = b"determinism";
        let a = hash_bytes(data);
        let b = hash_bytes(data);
        let c = hash_bytes(data);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn hash_output_is_64_hex_chars() {
        let h = hash_bytes(b"anything");
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
