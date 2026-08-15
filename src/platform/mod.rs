//! Platform-specific filesystem primitives.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::CStr;

use rustix::fd::AsFd;
use rustix::io::{self, Errno};

#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

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
