//! Platform-specific filesystem primitives.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
use std::cell::Cell;
use std::ffi::CStr;

use rustix::fd::{AsFd, OwnedFd};
use rustix::io::{self, Errno};

#[cfg(target_os = "linux")]
use rustix::fs::{Mode, OFlags};

#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, FromRawFd};

#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

pub(crate) mod directory;
pub(crate) mod spool;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_PARENT_DIRECTORY_SYNC: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_parent_directory_sync_for_test() {
    FAIL_NEXT_PARENT_DIRECTORY_SYNC.with(|failure| failure.set(true));
}

/// The timestamp grid reported by a platform-specific filesystem query.
///
/// Metadata policy owns the interpretation; this platform boundary owns the
/// raw `fpathconf`/`fstatfs` details and never makes an unknown result look
/// precise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimestampResolutionQuery {
    Known(u64),
    Unknown,
}

/// Query a destination filesystem's timestamp representation from an open
/// descriptor. Unsupported or indeterminate filesystems return `Unknown`.
pub(crate) fn timestamp_resolution<Fd: AsFd>(fd: Fd) -> io::Result<TimestampResolutionQuery> {
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
        return Ok(TimestampResolutionQuery::Unknown);
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
        // SAFETY: `fd` is live and the POSIX constant is defined on exactly
        // these selected targets. A non-positive result is indeterminate.
        let value = unsafe {
            libc::fpathconf(
                std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()),
                libc::_PC_TIMESTAMP_RESOLUTION,
            )
        };
        return Ok(if value > 0 {
            TimestampResolutionQuery::Known(value as u64)
        } else {
            TimestampResolutionQuery::Unknown
        });
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
        Ok(TimestampResolutionQuery::Unknown)
    }
}

/// Open a symlink itself after it has just been created, so an error while
/// recording its identity can still be recovered through a held descriptor.
///
/// Linux exposes this with `O_PATH|O_NOFOLLOW`; Darwin exposes it with
/// `O_SYMLINK`. Callers must still `fstat` and validate that the descriptor is
/// a symlink before treating it as an ownership capability.
pub(crate) fn open_created_symlink<P: AsFd>(parent: P, name: &CStr) -> std::io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::openat(
            parent,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)
    }

    #[cfg(target_os = "macos")]
    {
        // SAFETY: `parent` is a live descriptor and `name` is an owned,
        // NUL-terminated one-component C string. The returned descriptor is
        // transferred to `OwnedFd` exactly once on success.
        let raw = unsafe {
            libc::openat(
                parent.as_fd().as_raw_fd(),
                name.as_ptr(),
                libc::O_SYMLINK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: `raw` is a newly-owned descriptor returned by `openat`.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (parent, name);
        Err(std::io::Error::from_raw_os_error(libc::ENOTSUP))
    }
}

#[cfg(target_os = "linux")]
fn linux_timestamp_resolution<Fd: AsFd>(fd: Fd) -> io::Result<TimestampResolutionQuery> {
    const EXT4_SUPER_MAGIC: u64 = 0x0000_ef53;
    const BTRFS_SUPER_MAGIC: u64 = 0x9123_683e;
    const XFS_SUPER_MAGIC: u64 = 0x5846_5342;
    const F2FS_SUPER_MAGIC: u64 = 0xf2f5_2010;
    const EROFS_SUPER_MAGIC: u64 = 0xe0f5_e1e2;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
    const MSDOS_SUPER_MAGIC: u64 = 0x0000_4d44;
    const EXFAT_SUPER_MAGIC: u64 = 0x2011_bab0;

    // SAFETY: `statfs` is initialized by `fstatfs` on success and is never
    // read on failure.
    let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
    // SAFETY: `fd` is live and `statfs` has the exact writable ABI layout.
    let result =
        unsafe { libc::fstatfs(std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()), &mut statfs) };
    if result != 0 {
        return Ok(TimestampResolutionQuery::Unknown);
    }
    Ok(match statfs.f_type as u64 {
        EXT4_SUPER_MAGIC
        | BTRFS_SUPER_MAGIC
        | XFS_SUPER_MAGIC
        | F2FS_SUPER_MAGIC
        | EROFS_SUPER_MAGIC
        | TMPFS_MAGIC
        | OVERLAYFS_SUPER_MAGIC => TimestampResolutionQuery::Known(1),
        MSDOS_SUPER_MAGIC | EXFAT_SUPER_MAGIC => TimestampResolutionQuery::Known(2_000_000_000),
        _ => TimestampResolutionQuery::Unknown,
    })
}

#[cfg(target_os = "macos")]
fn macos_timestamp_resolution<Fd: AsFd>(fd: Fd) -> io::Result<TimestampResolutionQuery> {
    // SAFETY: `statfs` is initialized by `fstatfs` on success and is never
    // read on failure.
    let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
    // SAFETY: `fd` is live and `statfs` has the exact writable ABI layout.
    let result =
        unsafe { libc::fstatfs(std::os::fd::AsRawFd::as_raw_fd(&fd.as_fd()), &mut statfs) };
    if result != 0 {
        return Ok(TimestampResolutionQuery::Unknown);
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
        b"apfs" | b"hfs" => TimestampResolutionQuery::Known(1),
        _ => TimestampResolutionQuery::Unknown,
    })
}

/// Which descriptor-based data path produced a temporary destination.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyMethod {
    /// APFS (or another Darwin filesystem) cloned the source extents.
    Clone,
    /// Linux `FICLONE` created a reflink to the source extents.
    Reflink,
    /// Linux `copy_file_range` copied data in-kernel.
    CopyFileRange,
    /// The portable read/write loop copied data through the caller's buffer.
    Buffered,
}

/// Failure from an optional acceleration attempt.
///
/// `Unsupported` is deliberately distinct from `Io`: callers may fall back
/// only for the former.  Permission failures, invalid descriptors, and other
/// per-file errors must remain visible rather than being mistaken for a
/// filesystem capability result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyAttemptError {
    Unsupported,
    Io(Errno),
}

/// Flush an unpublished regular file for `--durable` publication.
///
/// Apple distinguishes ordinary `fsync` from `F_FULLFSYNC`; use the latter
/// where it is available.  The parent directory is synchronized separately
/// after rename, because the namespace mutation is a distinct durability
/// boundary.
pub(crate) fn sync_file_for_durable_publish<Fd: AsFd>(fd: Fd) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        rustix::fs::fcntl_fullfsync(fd)
    }

    #[cfg(not(target_os = "macos"))]
    {
        rustix::fs::fsync(fd)
    }
}

/// Persist the containing directory after a namespace publication.
pub(crate) fn sync_parent_directory<Fd: AsFd>(fd: Fd) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_PARENT_DIRECTORY_SYNC.with(|failure| failure.replace(false)) {
        return Err(Errno::IO);
    }
    rustix::fs::fsync(fd)
}

/// Persist directory metadata and directory-entry changes. Directories use
/// ordinary `fsync` even on macOS: `F_FULLFSYNC` is the regular-file data path
/// and is not a portable directory operation.
pub(crate) fn sync_directory_for_durable_metadata<Fd: AsFd>(fd: Fd) -> io::Result<()> {
    rustix::fs::fsync(fd)
}

impl CopyAttemptError {
    fn from_errno(errno: Errno) -> Self {
        if is_structural_unsupported(errno) {
            Self::Unsupported
        } else {
            Self::Io(errno)
        }
    }
}

impl std::fmt::Display for CopyAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("copy acceleration is unsupported"),
            Self::Io(errno) => write!(f, "copy acceleration failed: {errno}"),
        }
    }
}

impl std::error::Error for CopyAttemptError {}

/// Attempt an FD-relative clone into a new destination name.
///
/// On macOS the destination name is created by `fclonefileat`; on Linux this
/// operation has no pathname equivalent and returns `Unsupported`.  The
/// destination name must be a single, caller-owned component and must not
/// already exist.
#[cfg(target_os = "macos")]
pub(crate) fn try_clone_to_dir<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst_dir: DstFd,
    dst_name: &CStr,
) -> Result<(), CopyAttemptError> {
    macos::try_clone_to_dir(src, dst_dir, dst_name).map_err(CopyAttemptError::from_errno)
}

/// Attempt Linux's descriptor-to-descriptor reflink operation.
///
/// The destination must already be an exclusively-created temporary regular
/// file.  The descriptor offsets are not changed by `FICLONE`.
#[cfg(target_os = "linux")]
pub(crate) fn try_reflink<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst: DstFd,
) -> Result<(), CopyAttemptError> {
    linux::try_reflink(src, dst).map_err(CopyAttemptError::from_errno)
}

/// Attempt Linux's in-kernel `copy_file_range` loop.
///
/// The function copies until the source reports EOF and leaves descriptor
/// offsets unchanged.  If the first operation reports a structural
/// unsupported error, callers may safely use [`buffered_copy`].  Once bytes
/// have been copied, the same error is returned as a hard failure so callers
/// cannot accidentally append a buffered copy to a partially populated temp.
#[cfg(target_os = "linux")]
pub(crate) fn try_copy_file_range<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst: DstFd,
) -> Result<u64, CopyAttemptError> {
    linux::try_copy_file_range(src, dst).map_err(|error| match error {
        linux::CopyRangeError::Unsupported => CopyAttemptError::Unsupported,
        linux::CopyRangeError::Io(errno) => CopyAttemptError::Io(errno),
    })
}

/// Copy bytes through a caller-owned reusable buffer.
///
/// Both descriptors are consumed from their current offsets.  Short reads,
/// short writes, and `EINTR` are handled here so all platform engines share
/// one conservative fallback implementation.
pub(crate) fn buffered_copy<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst: DstFd,
    scratch: &mut [u8],
) -> io::Result<u64> {
    if scratch.is_empty() {
        return Err(Errno::INVAL);
    }

    let mut copied = 0_u64;
    loop {
        let read = loop {
            match rustix::io::read(&src, &mut *scratch) {
                Err(Errno::INTR) => continue,
                result => break result?,
            }
        };
        if read == 0 {
            return Ok(copied);
        }

        let mut written = 0;
        while written < read {
            let n = loop {
                match rustix::io::write(&dst, &scratch[written..read]) {
                    Err(Errno::INTR) => continue,
                    result => break result?,
                }
            };
            if n == 0 {
                return Err(Errno::IO);
            }
            written += n;
        }
        copied += read as u64;
    }
}

/// Errors which describe a filesystem pair that cannot provide an optional
/// acceleration.  Do not add permission or I/O errors here: callers use this
/// distinction to decide whether fallback is safe.
fn is_structural_unsupported(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::XDEV | Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP
    )
}
