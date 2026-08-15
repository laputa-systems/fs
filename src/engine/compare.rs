//! Source/destination comparison and equality proofs.
//!
//! Comparison is intentionally a proof about already-open descriptors and
//! caller-captured [`FileStamp`]s.  Namespace lookup and mutation belong to
//! the surrounding engine; this module never reopens a path.

use std::io;

use rustix::fd::AsFd;

use crate::hash::{HashError, hash_fd};
use crate::metadata::{
    FileKind, FileStamp, TimestampComparison, TimestampResolution, compare_timestamps,
};

/// The externally selected data equality proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckMode {
    /// Compare size and normalized mtime.  Unknown timestamp precision is
    /// resolved by hashing only that ambiguous same-size case.
    Metadata,
    /// Compare size and BLAKE3 digest; mtime is not a content proof.
    Hash,
}

/// Result of comparing corresponding regular files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegularFileComparison {
    Equal,
    Different,
}

#[derive(Debug)]
pub(crate) enum CompareError {
    Io(io::Error),
    Hash(HashError),
    NotRegular {
        source: FileKind,
        destination: FileKind,
    },
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Hash(error) => error.fmt(f),
            Self::NotRegular {
                source,
                destination,
            } => write!(
                f,
                "regular-file comparison received {source:?} and {destination:?}"
            ),
        }
    }
}

impl std::error::Error for CompareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Hash(error) => Some(error),
            Self::NotRegular { .. } => None,
        }
    }
}

impl From<io::Error> for CompareError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Compare corresponding regular files.
///
/// Descriptors must be positioned at offset zero when `check == Hash`; the
/// descriptor-based hash helper intentionally consumes the descriptor's
/// current position.  The normal engine opens each descriptor immediately
/// before this operation, so this is both explicit and allocation-light.
pub(crate) fn compare_regular_files<S: AsFd, D: AsFd>(
    source: S,
    destination: D,
    source_stamp: FileStamp,
    destination_stamp: FileStamp,
    check: CheckMode,
    destination_resolution: TimestampResolution,
    scratch: &mut [u8],
) -> Result<RegularFileComparison, CompareError> {
    if !source_stamp.file_type.is_regular() || !destination_stamp.file_type.is_regular() {
        return Err(CompareError::NotRegular {
            source: source_stamp.file_type,
            destination: destination_stamp.file_type,
        });
    }

    if source_stamp.size != destination_stamp.size {
        return Ok(RegularFileComparison::Different);
    }

    if check == CheckMode::Metadata {
        match compare_timestamps(
            source_stamp.mtime,
            destination_stamp.mtime,
            destination_resolution,
        ) {
            TimestampComparison::Equal => return Ok(RegularFileComparison::Equal),
            TimestampComparison::Different => return Ok(RegularFileComparison::Different),
            // An unknown representation cannot prove inequality.  Continue
            // to a content proof below.
            TimestampComparison::Ambiguous => {}
        }
    }

    if scratch.is_empty() {
        return Err(CompareError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hash comparison requires a non-empty scratch buffer",
        )));
    }

    let source_hash = hash_fd(&source, source_stamp.size, scratch).map_err(CompareError::Hash)?;
    // `hash_fd` consumes the provided descriptor from its current offset.  A
    // caller that passes independent freshly-opened descriptors (the engine's
    // normal path) therefore gets a second independent stream here.
    let destination_hash =
        hash_fd(&destination, destination_stamp.size, scratch).map_err(CompareError::Hash)?;
    Ok(if source_hash == destination_hash {
        RegularFileComparison::Equal
    } else {
        RegularFileComparison::Different
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{FileKind, stamp_fd};
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn pair(left: &[u8], right: &[u8]) -> (std::fs::File, std::fs::File) {
        let mut source = fixture("source");
        let mut destination = fixture("destination");
        source.write_all(left).unwrap();
        destination.write_all(right).unwrap();
        source.seek(SeekFrom::Start(0)).unwrap();
        destination.seek(SeekFrom::Start(0)).unwrap();
        (source, destination)
    }

    fn fixture(label: &str) -> std::fs::File {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-compare-{label}-{}-{nonce}-{counter}",
            std::process::id()
        ));
        std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .inspect(|_file| {
                let _ = std::fs::remove_file(path);
            })
            .unwrap()
    }

    #[test]
    fn metadata_mode_hashes_only_ambiguous_mtime() {
        let (source, destination) = pair(b"same", b"same");
        let mut source_stamp = stamp_fd(&source).unwrap();
        let mut destination_stamp = stamp_fd(&destination).unwrap();
        source_stamp.mtime = crate::metadata::Timestamp::new(1, 1);
        destination_stamp.mtime = crate::metadata::Timestamp::new(1, 2);
        let mut scratch = [0u8; 16];
        assert_eq!(
            compare_regular_files(
                &source,
                &destination,
                source_stamp,
                destination_stamp,
                CheckMode::Metadata,
                TimestampResolution::Unknown,
                &mut scratch,
            )
            .unwrap(),
            RegularFileComparison::Equal
        );
    }

    #[test]
    fn known_timestamp_mismatch_is_data_difference_without_a_content_read() {
        let (source, destination) = pair(b"same", b"same");
        let mut source_stamp = stamp_fd(&source).unwrap();
        let mut destination_stamp = stamp_fd(&destination).unwrap();
        source_stamp.mtime = crate::metadata::Timestamp::new(2, 0);
        destination_stamp.mtime = crate::metadata::Timestamp::new(1, 0);
        let mut scratch = [0u8; 16];
        assert_eq!(
            compare_regular_files(
                &source,
                &destination,
                source_stamp,
                destination_stamp,
                CheckMode::Metadata,
                TimestampResolution::known(1).unwrap(),
                &mut scratch,
            )
            .unwrap(),
            RegularFileComparison::Different
        );
    }

    #[test]
    fn hash_mode_ignores_mtime_when_contents_match() {
        let (source, destination) = pair(b"same", b"same");
        let mut source_stamp = stamp_fd(&source).unwrap();
        let mut destination_stamp = stamp_fd(&destination).unwrap();
        source_stamp.mtime = crate::metadata::Timestamp::new(2, 0);
        destination_stamp.mtime = crate::metadata::Timestamp::new(1, 0);
        let mut scratch = [0u8; 16];
        assert_eq!(
            compare_regular_files(
                &source,
                &destination,
                source_stamp,
                destination_stamp,
                CheckMode::Hash,
                TimestampResolution::known(1).unwrap(),
                &mut scratch,
            )
            .unwrap(),
            RegularFileComparison::Equal
        );
    }

    #[test]
    fn rejects_non_regular_objects() {
        let stamp = FileStamp {
            dev: 1,
            ino: 1,
            file_type: FileKind::Directory,
            size: 0,
            mode: 0o40755,
            mtime: crate::metadata::Timestamp::default(),
            ctime: crate::metadata::Timestamp::default(),
        };
        let regular = FileStamp {
            file_type: FileKind::Regular,
            ..stamp
        };
        let source = fixture("source");
        let destination = fixture("destination");
        let mut scratch = [0u8; 1];
        assert!(matches!(
            compare_regular_files(
                &source,
                &destination,
                stamp,
                regular,
                CheckMode::Metadata,
                TimestampResolution::known(1).unwrap(),
                &mut scratch,
            ),
            Err(CompareError::NotRegular { .. })
        ));
    }
}
