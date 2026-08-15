//! Metadata comparison, propagation, and timestamp handling.
//!
//! The engine keeps directory descriptors open while it walks a tree.  This
//! module deliberately works on those descriptors (and on one-component
//! `*at` lookups) rather than accepting reconstructed paths.  In particular,
//! The engine works entirely from already-open descriptors; this module does
//! not reconstruct paths to inspect or mutate metadata.

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
    pub const NANOS: Self = Self::Known(1);

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
        #[cfg(target_os = "linux")]
        {
            return linux_timestamp_resolution(fd);
        }

        #[cfg(target_os = "macos")]
        {
            macos_timestamp_resolution(fd)
        }

        #[cfg(target_os = "freebsd")]
        {
            let _ = fd;
            return Ok(Self::Unknown);
        }

        #[cfg(any(
            target_os = "aix",
            target_os = "cygwin",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "illumos",
            target_os = "solaris"
        ))]
        {
            // POSIX specifies this value in nanoseconds.  `-1` can mean an
            // error or an indeterminate value; both are treated as unknown.
            // SAFETY: `fd` is a live borrowed descriptor and the POSIX
            // constant is defined on these selected targets.
            let value = unsafe {
                libc::fpathconf(
                    std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()),
                    libc::_PC_TIMESTAMP_RESOLUTION,
                )
            };
            if value > 0 {
                return Ok(Self::known(value as u64).unwrap_or(Self::Unknown));
            }
            return Ok(Self::Unknown);
        }

        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "aix",
            target_os = "cygwin",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "illumos",
            target_os = "solaris"
        )))]
        {
            let _ = fd;
            Ok(Self::Unknown)
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_timestamp_resolution<Fd: AsFd>(fd: Fd) -> io::Result<TimestampResolution> {
    // Linux's libc has no `_PC_TIMESTAMP_RESOLUTION`.  The values below are
    // the stable `statfs(2)` magic numbers for filesystems for which V1 has a
    // deterministic timestamp grid.  FAT and exFAT deliberately use their
    // coarse, documented two-second mtime grid.
    const EXT4_SUPER_MAGIC: u64 = 0x0000_ef53;
    const BTRFS_SUPER_MAGIC: u64 = 0x9123_683e;
    const XFS_SUPER_MAGIC: u64 = 0x5846_5342;
    const F2FS_SUPER_MAGIC: u64 = 0xf2f5_2010;
    const EROFS_SUPER_MAGIC: u64 = 0xe0f5_e1e2;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
    const MSDOS_SUPER_MAGIC: u64 = 0x0000_4d44;
    const EXFAT_SUPER_MAGIC: u64 = 0x2011_bab0;

    // SAFETY: `libc::statfs` is a plain output struct and all-zero is a valid
    // pre-call state; `fstatfs` initializes it on a successful return.
    let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
    // SAFETY: `fd` is live for the call and `statfs` is a valid writable
    // pointer to the exact C structure expected by the platform ABI.
    let result =
        unsafe { libc::fstatfs(std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()), &mut statfs) };
    if result != 0 {
        return Ok(TimestampResolution::Unknown);
    }

    let magic = statfs.f_type as u64;
    Ok(match magic {
        EXT4_SUPER_MAGIC
        | BTRFS_SUPER_MAGIC
        | XFS_SUPER_MAGIC
        | F2FS_SUPER_MAGIC
        | EROFS_SUPER_MAGIC
        | TMPFS_MAGIC
        | OVERLAYFS_SUPER_MAGIC => TimestampResolution::NANOS,
        MSDOS_SUPER_MAGIC | EXFAT_SUPER_MAGIC => TimestampResolution::known(2_000_000_000).unwrap(),
        _ => TimestampResolution::Unknown,
    })
}

#[cfg(target_os = "macos")]
fn macos_timestamp_resolution<Fd: AsFd>(fd: Fd) -> io::Result<TimestampResolution> {
    // SAFETY: `libc::statfs` is a plain output struct and all-zero is a valid
    // pre-call state; `fstatfs` initializes it on a successful return.
    let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
    // SAFETY: `fd` is live for the call and `statfs` is a valid writable
    // pointer to the exact C structure expected by the platform ABI.
    let result =
        unsafe { libc::fstatfs(std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()), &mut statfs) };
    if result != 0 {
        return Ok(TimestampResolution::Unknown);
    }
    let name_len = statfs
        .f_fstypename
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(statfs.f_fstypename.len());
    let name = statfs.f_fstypename[..name_len]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    Ok(match name.as_slice() {
        b"apfs" | b"hfs" => TimestampResolution::NANOS,
        _ => TimestampResolution::Unknown,
    })
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

fn xattr_list<Fd: AsFd>(fd: Fd) -> io::Result<Vec<Vec<u8>>> {
    let mut capacity = 4096usize;
    loop {
        let mut buffer = vec![0u8; capacity];
        match fs::flistxattr(fd.as_fd(), &mut buffer[..]) {
            Ok(length) => {
                let bytes = &buffer[..length];
                let mut names = Vec::new();
                for name in bytes
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                {
                    let mut nul_terminated = Vec::with_capacity(name.len() + 1);
                    nul_terminated.extend_from_slice(name);
                    nul_terminated.push(0);
                    // Keep the terminator in each name so it can be passed to
                    // rustix without lossy UTF-8 conversion.
                    names.push(nul_terminated);
                }
                return Ok(names);
            }
            Err(error) if error.raw_os_error() == libc::ERANGE && capacity < 16 * 1024 * 1024 => {
                capacity *= 2;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn xattr_value<Fd: AsFd>(fd: Fd, name: &CStr) -> io::Result<Vec<u8>> {
    let mut capacity = 4096usize;
    loop {
        let mut buffer = vec![0u8; capacity];
        match fs::fgetxattr(fd.as_fd(), name, &mut buffer[..]) {
            Ok(length) => return Ok(buffer[..length].to_vec()),
            Err(error) if error.raw_os_error() == libc::ERANGE && capacity < 16 * 1024 * 1024 => {
                capacity *= 2;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Propagate all readable source xattrs to an already-open destination object.
///
/// This intentionally does not inspect the destination first and does not
/// remove destination-only attributes.  V1 promises propagation on creation or
/// another metadata mutation, not exact xattr equality on the no-op path.
pub fn propagate_xattrs<S: AsFd, D: AsFd>(source: S, destination: D) -> io::Result<usize> {
    let names = xattr_list(source.as_fd())?;
    let mut copied = 0;
    for name in names {
        let c_name = CStr::from_bytes_with_nul(&name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem returned malformed xattr name",
            )
        })?;
        let value = xattr_value(source.as_fd(), c_name)?;
        fs::fsetxattr(destination.as_fd(), c_name, &value, fs::XattrFlags::empty())?;
        copied += 1;
    }
    Ok(copied)
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
            TimestampResolution::NANOS
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
