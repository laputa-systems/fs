//! BLAKE3 content hashing.
//!
//! Hashing is deliberately descriptor based.  The engine has already opened
//! and checked a source object by the time it needs a content comparison, so
//! reopening a path here would reintroduce a namespace race.  A reusable
//! caller-owned buffer keeps the normal hash path allocation-free.

use rustix::fd::AsFd;
use rustix::io::Errno;

/// Hash exactly `size` bytes from the current position of `fd`.
///
/// The caller should pass a descriptor positioned at the beginning of the
/// object and a non-empty reusable scratch buffer.  Reading exactly the
/// stat-observed size makes a concurrent truncation an explicit error rather
/// than silently producing a hash for a different object.  A concurrent
/// extension is intentionally outside this function's contract; the engine's
/// source-stability check handles that case before publication.
pub(crate) fn hash_fd<Fd: AsFd>(
    fd: Fd,
    size: u64,
    scratch: &mut [u8],
) -> Result<blake3::Hash, HashError> {
    if scratch.is_empty() {
        return Err(HashError::Io(Errno::INVAL));
    }

    let mut hasher = blake3::Hasher::new();
    let mut remaining = size;

    while remaining != 0 {
        let request = remaining.min(scratch.len() as u64) as usize;
        let read = read_retry(&fd, &mut scratch[..request]).map_err(HashError::Io)?;
        if read == 0 {
            return Err(HashError::UnexpectedEof {
                expected: size,
                observed: size - remaining,
            });
        }

        hasher.update(&scratch[..read]);
        remaining -= read as u64;
    }

    Ok(hasher.finalize())
}

/// Errors returned by [`hash_fd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashError {
    /// The descriptor read failed with this operating-system error.
    Io(Errno),
    /// The source was truncated before its stat-observed size was read.
    UnexpectedEof { expected: u64, observed: u64 },
}

impl std::fmt::Display for HashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(errno) => write!(f, "hash read failed: {errno}"),
            Self::UnexpectedEof { expected, observed } => write!(
                f,
                "source truncated while hashing (expected {expected} bytes, read {observed})"
            ),
        }
    }
}

impl std::error::Error for HashError {}

fn read_retry<Fd: AsFd>(fd: &Fd, buffer: &mut [u8]) -> rustix::io::Result<usize> {
    loop {
        match rustix::io::read(fd, &mut *buffer) {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::os::fd::AsFd;

    #[test]
    fn hashes_descriptor_contents_without_reopening_a_path() {
        let mut file = tempfile_for_test();
        let contents = b"descriptor hashing keeps the namespace out of the loop";
        file.write_all(contents).expect("write test contents");
        file.flush().expect("flush test contents");
        file.rewind().expect("rewind test file");

        let mut scratch = [0_u8; 7];
        let actual = hash_fd(file.as_fd(), contents.len() as u64, &mut scratch)
            .expect("hash should succeed");
        let expected = blake3::hash(contents);
        assert_eq!(actual, expected);
    }

    #[test]
    fn reports_truncation_instead_of_hashing_a_short_file() {
        let mut file = tempfile_for_test();
        file.write_all(b"short").expect("write test contents");
        file.rewind().expect("rewind test file");

        let mut scratch = [0_u8; 8];
        let error = hash_fd(file.as_fd(), 10, &mut scratch).expect_err("must report EOF");
        assert!(matches!(error, HashError::UnexpectedEof { .. }));
    }

    fn tempfile_for_test() -> std::fs::File {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("fs-hash-test-{}-{nonce}", std::process::id()));
        std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .inspect(|file| {
                // Keep the fixture self-cleaning without requiring a temp-file
                // dependency in the production crate.
                let _ = std::fs::remove_file(path);
            })
            .expect("create hash test file")
    }
}
