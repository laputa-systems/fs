//! Metadata comparison, propagation, and timestamp handling.
//!
//! The engine keeps directory descriptors open while it walks a tree.  This
//! module deliberately works on those descriptors (and on one-component
//! `*at` lookups) rather than accepting reconstructed paths.  In particular,
//! The engine works entirely from already-open descriptors; this module does
//! not reconstruct paths to inspect or mutate metadata.

use std::cell::RefCell;
use std::ffi::CStr;
use std::io;
use std::os::fd::AsFd;

use rustix::fs::{self, FileType, Mode, Timespec};

/// A timestamp as returned by `stat`.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl Timestamp {
    pub const fn new(seconds: i64, nanoseconds: u32) -> Self {
        Self {
            seconds,
            nanoseconds,
        }
    }

    /// Return the timestamp as a signed number of nanoseconds from the Unix
    /// epoch.  `i128` keeps the arithmetic safe for all representable `time_t`
    /// values and for filesystem resolutions down to one nanosecond.
    pub fn total_nanoseconds(self) -> i128 {
        i128::from(self.seconds) * 1_000_000_000 + i128::from(self.nanoseconds)
    }

    pub fn from_total_nanoseconds(total: i128) -> io::Result<Self> {
        let seconds = total.div_euclid(1_000_000_000);
        let nanoseconds = total.rem_euclid(1_000_000_000);
        let seconds = i64::try_from(seconds).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "timestamp is outside time_t range",
            )
        })?;
        Ok(Self {
            seconds,
            nanoseconds: u32::try_from(nanoseconds).expect("euclidean remainder is < 1e9"),
        })
    }

    fn as_timespec(self) -> Timespec {
        Timespec {
            tv_sec: self.seconds,
            tv_nsec: self.nanoseconds.into(),
        }
    }
}

/// The object kind captured by [`FileStamp`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    Other,
}

impl FileKind {
    pub const fn is_regular(self) -> bool {
        matches!(self, Self::Regular)
    }
}

impl From<FileType> for FileKind {
    fn from(value: FileType) -> Self {
        match value {
            FileType::RegularFile => Self::Regular,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            FileType::Fifo => Self::Fifo,
            FileType::Socket => Self::Socket,
            FileType::CharacterDevice => Self::CharacterDevice,
            FileType::BlockDevice => Self::BlockDevice,
            FileType::Unknown => Self::Other,
        }
    }
}

/// Identity and mutation metadata captured from one filesystem object.
///
/// `dev`, `ino`, and `file_type` are the identity portion.  `size`, `mode`,
/// `mtime`, and `ctime` are the mutation stamp.  `mode` is the raw `st_mode`
/// value; use [`FileStamp::permission_bits`] when passing it to `fchmod`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileStamp {
    pub dev: u64,
    pub ino: u64,
    pub file_type: FileKind,
    pub size: u64,
    pub mode: u32,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
}

impl FileStamp {
    pub const fn permission_bits(self) -> u32 {
        self.mode & 0o7777
    }

    #[cfg(test)]
    pub fn same_identity(self, other: Self) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.file_type == other.file_type
    }

    #[cfg(test)]
    pub fn same_mutation_stamp(self, other: Self) -> bool {
        self.size == other.size
            && self.mode == other.mode
            && self.mtime == other.mtime
            && self.ctime == other.ctime
    }
}

fn stamp_from_stat(stat: fs::Stat) -> io::Result<FileStamp> {
    if stat.st_size < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem returned a negative file size",
        ));
    }
    Ok(FileStamp {
        dev: stat.st_dev as u64,
        ino: stat.st_ino,
        file_type: FileKind::from(FileType::from_raw_mode(stat.st_mode)),
        size: stat.st_size as u64,
        mode: stat.st_mode as u32,
        mtime: Timestamp::new(stat.st_mtime, stat.st_mtime_nsec as u32),
        ctime: Timestamp::new(stat.st_ctime, stat.st_ctime_nsec as u32),
    })
}

/// Capture metadata for an already-open descriptor.
pub fn stamp_fd<Fd: AsFd>(fd: Fd) -> io::Result<FileStamp> {
    stamp_from_stat(fs::fstat(fd)?)
}

/// The precision of timestamps representable by a destination filesystem.
///
/// `Known` is in nanoseconds and is scoped by the caller to one invocation
/// (and, where needed, one destination mount).  `Unknown` is not an excuse to
/// repeatedly mutate metadata: same-size files with differing mtimes require
/// a content proof before they are recopied.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimestampResolution {
    Known(u64),
    Unknown,
}

impl TimestampResolution {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const fn known(nanoseconds: u64) -> Option<Self> {
        // POSIX reports this value in nanoseconds.  Coarse filesystems can
        // legitimately have a resolution larger than one second (FAT and
        // exFAT use a two-second mtime grid), so only zero is invalid here.
        if nanoseconds == 0 {
            None
        } else {
            Some(Self::Known(nanoseconds))
        }
    }

    /// Normalize a timestamp to the destination's representable grid.  Floor
    /// normalization (including for pre-epoch timestamps) is deterministic
    /// and avoids the endless recopy loop caused by raw nanosecond equality.
    pub fn normalize(self, timestamp: Timestamp) -> Timestamp {
        match self {
            Self::Known(resolution) => {
                let resolution = i128::from(resolution);
                let total = timestamp.total_nanoseconds();
                let normalized = total.div_euclid(resolution) * resolution;
                Timestamp::from_total_nanoseconds(normalized)
                    .expect("normalizing a representable timestamp cannot overflow")
            }
            Self::Unknown => timestamp,
        }
    }

    pub fn equivalent(self, left: Timestamp, right: Timestamp) -> bool {
        self.normalize(left) == self.normalize(right)
    }

    /// Query the resolution for a destination descriptor.
    ///
    /// On platforms exposing POSIX `_PC_TIMESTAMP_RESOLUTION` we query it
    /// through the descriptor.  Linux and Darwin do not expose that POSIX
    /// name in their libc headers, so we classify only filesystems whose
    /// timestamp behavior is known and conservatively return `Unknown` for
    /// everything else.  In particular this prevents an exFAT/FAT mount from
    /// being mistaken for a nanosecond filesystem.
    pub fn for_fd<Fd: AsFd>(fd: Fd) -> io::Result<Self> {
        Ok(match crate::platform::timestamp_resolution(fd)? {
            crate::platform::TimestampResolutionQuery::Known(nanoseconds) => {
                Self::known(nanoseconds).unwrap_or(Self::Unknown)
            }
            crate::platform::TimestampResolutionQuery::Unknown => Self::Unknown,
        })
    }
}

/// Compare timestamps without making unknown precision look like inequality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimestampComparison {
    Equal,
    Different,
    Ambiguous,
}

pub fn compare_timestamps(
    left: Timestamp,
    right: Timestamp,
    resolution: TimestampResolution,
) -> TimestampComparison {
    match resolution {
        TimestampResolution::Known(_) => {
            if resolution.equivalent(left, right) {
                TimestampComparison::Equal
            } else {
                TimestampComparison::Different
            }
        }
        TimestampResolution::Unknown => {
            if left == right {
                TimestampComparison::Equal
            } else {
                TimestampComparison::Ambiguous
            }
        }
    }
}

/// Apply permission bits to an open object.
pub fn set_mode_fd<Fd: AsFd>(fd: Fd, raw_mode: u32) -> io::Result<()> {
    Ok(fs::fchmod(
        fd,
        Mode::from_raw_mode((raw_mode & 0o7777) as _),
    )?)
}

/// Apply mtime while leaving atime untouched.  Call this after child creation
/// or deletion: either operation invalidates a directory's mtime.
pub fn set_mtime_fd<Fd: AsFd>(fd: Fd, mtime: Timestamp) -> io::Result<()> {
    Ok(fs::futimens(
        fd,
        &fs::Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: fs::UTIME_OMIT,
            },
            last_modification: mtime.as_timespec(),
        },
    )?)
}

const INITIAL_XATTR_BUFFER_SIZE: usize = 4096;
const MAX_XATTR_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Reusable storage for one xattr propagation operation.
///
/// The list buffer contains the NUL-separated names returned by
/// `flistxattr`; names are borrowed directly from that buffer. The value
/// buffer is reused for each `fgetxattr`/`fsetxattr` pair. Keeping this state
/// with a worker avoids allocating a name and value vector for every
/// metadata mutation while retaining the fd-relative API.
#[derive(Debug)]
pub(crate) struct XattrScratch {
    list: Vec<u8>,
    value: Vec<u8>,
}

impl Default for XattrScratch {
    fn default() -> Self {
        Self {
            list: vec![0; INITIAL_XATTR_BUFFER_SIZE],
            value: vec![0; INITIAL_XATTR_BUFFER_SIZE],
        }
    }
}

impl XattrScratch {
    fn grow(buffer: &mut Vec<u8>) -> io::Result<()> {
        if buffer.len() >= MAX_XATTR_BUFFER_SIZE {
            return Err(io::Error::from_raw_os_error(libc::ERANGE));
        }
        let next = buffer.len().saturating_mul(2).min(MAX_XATTR_BUFFER_SIZE);
        if next <= buffer.len() {
            return Err(io::Error::from_raw_os_error(libc::ERANGE));
        }
        buffer.resize(next, 0);
        Ok(())
    }
}

thread_local! {
    /// The compatibility wrapper below is used by metadata call sites that
    /// do not carry an explicit worker scratch object. Keeping one scratch
    /// per calling thread preserves the no-per-mutation-allocation property
    /// without introducing a global lock.
    static DEFAULT_XATTR_SCRATCH: RefCell<XattrScratch> =
        RefCell::new(XattrScratch::default());
}

/// Propagate all readable source xattrs to an already-open destination object.
///
/// This intentionally does not inspect the destination first and does not
/// remove destination-only attributes.  V1 promises propagation on creation or
/// another metadata mutation, not exact xattr equality on the no-op path.
pub(crate) fn propagate_xattrs_with_scratch<S: AsFd, D: AsFd>(
    source: S,
    destination: D,
    scratch: &mut XattrScratch,
) -> io::Result<usize> {
    let list_length = loop {
        match fs::flistxattr(source.as_fd(), &mut scratch.list[..]) {
            Ok(length) => break length,
            Err(error) if error.raw_os_error() == libc::ERANGE => {
                XattrScratch::grow(&mut scratch.list)?;
            }
            Err(error) => return Err(error.into()),
        }
    };

    let mut copied = 0;
    let mut name_start = 0;
    while name_start < list_length {
        let relative_end = scratch.list[name_start..list_length]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem returned malformed xattr name list",
                )
            })?;
        let name_end = name_start + relative_end;
        if name_end == name_start {
            name_start += 1;
            continue;
        }
        let c_name =
            CStr::from_bytes_with_nul(&scratch.list[name_start..=name_end]).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem returned malformed xattr name",
                )
            })?;
        let value_length = loop {
            match fs::fgetxattr(source.as_fd(), c_name, &mut scratch.value[..]) {
                Ok(length) => break length,
                Err(error) if error.raw_os_error() == libc::ERANGE => {
                    XattrScratch::grow(&mut scratch.value)?;
                }
                Err(error) => return Err(error.into()),
            }
        };
        fs::fsetxattr(
            destination.as_fd(),
            c_name,
            &scratch.value[..value_length],
            fs::XattrFlags::empty(),
        )?;
        copied += 1;
        name_start = name_end + 1;
    }
    Ok(copied)
}

/// Propagate source xattrs using bounded per-thread scratch storage.
///
/// Callers that want explicit ownership of the buffers may instead retain an
/// [`XattrScratch`] and call [`propagate_xattrs_with_scratch`].
pub fn propagate_xattrs<S: AsFd, D: AsFd>(source: S, destination: D) -> io::Result<usize> {
    DEFAULT_XATTR_SCRATCH
        .with_borrow_mut(|scratch| propagate_xattrs_with_scratch(source, destination, scratch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_normalization_is_a_grid_and_handles_pre_epoch() {
        let resolution = TimestampResolution::known(1_000_000_000).unwrap();
        assert_eq!(
            resolution.normalize(Timestamp::new(12, 999_999_999)),
            Timestamp::new(12, 0)
        );
        assert_eq!(
            resolution.normalize(Timestamp::new(-1, 1)),
            Timestamp::new(-1, 0)
        );
        assert_eq!(
            resolution.normalize(Timestamp::new(-1, 999_999_999)),
            Timestamp::new(-1, 0)
        );
    }

    #[test]
    fn unknown_resolution_distinguishes_equal_from_ambiguous() {
        let a = Timestamp::new(1, 2);
        assert_eq!(
            compare_timestamps(a, a, TimestampResolution::Unknown),
            TimestampComparison::Equal
        );
        assert_eq!(
            compare_timestamps(a, Timestamp::new(1, 3), TimestampResolution::Unknown),
            TimestampComparison::Ambiguous
        );
    }

    #[test]
    fn two_second_filesystem_grid_prevents_recopy_churn() {
        let resolution = TimestampResolution::known(2_000_000_000).unwrap();
        let source = Timestamp::new(101, 999_999_999);
        let representable_destination = Timestamp::new(100, 0);
        assert!(resolution.equivalent(source, representable_destination));
        assert_eq!(
            compare_timestamps(source, representable_destination, resolution),
            TimestampComparison::Equal
        );
        assert_eq!(
            compare_timestamps(source, Timestamp::new(102, 0), resolution),
            TimestampComparison::Different
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_destination_resolution_is_deterministic() {
        let directory = std::fs::File::open(".").expect("open workspace directory");
        assert_eq!(
            TimestampResolution::for_fd(directory).unwrap(),
            TimestampResolution::known(1).unwrap()
        );
    }

    #[test]
    fn stamp_identity_excludes_mutation_fields() {
        let a = FileStamp {
            dev: 1,
            ino: 2,
            file_type: FileKind::Regular,
            size: 3,
            mode: 0o100644,
            mtime: Timestamp::new(4, 5),
            ctime: Timestamp::new(6, 7),
        };
        let mut b = a;
        b.size = 99;
        assert!(a.same_identity(b));
        assert!(!a.same_mutation_stamp(b));
    }
}
