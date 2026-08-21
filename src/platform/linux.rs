//! Linux filesystem primitives.

use rustix::fd::AsFd;
use rustix::io::{self, Errno};

/// `FICLONE` reflink into an exclusively-created destination temporary file.
pub(crate) fn try_reflink<SrcFd: AsFd, DstFd: AsFd>(src: SrcFd, dst: DstFd) -> io::Result<()> {
    // rustix owns the small ioctl wrapper and confines its `unsafe` to the
    // implementation of that wrapper.  Both descriptors remain borrowed for
    // the duration of the call and refer to regular files supplied by the
    // caller.
    rustix::fs::ioctl_ficlone(dst, src)
}

/// Error classification local to the copy-range loop.  A structural error
/// after a partial copy is intentionally promoted to `Io`; the caller must
/// not append a buffered copy to a partially populated destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyRangeError {
    Unsupported,
    Io(Errno),
}

/// Copy through Linux's in-kernel `copy_file_range` until source EOF.
pub(crate) fn try_copy_file_range<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst: DstFd,
) -> Result<u64, CopyRangeError> {
    const CHUNK: usize = 8 * 1024 * 1024;
    let mut input_offset = 0_u64;
    let mut output_offset = 0_u64;
    let mut copied = 0_u64;

    loop {
        let result = loop {
            match rustix::fs::copy_file_range(
                &src,
                Some(&mut input_offset),
                &dst,
                Some(&mut output_offset),
                CHUNK,
            ) {
                Err(Errno::INTR) => continue,
                result => break result,
            }
        };

        match result {
            Ok(0) => return Ok(copied),
            Ok(n) => {
                copied += n as u64;
            }
            Err(errno) if copied == 0 && is_structural_unsupported(errno) => {
                return Err(CopyRangeError::Unsupported);
            }
            Err(errno) => return Err(CopyRangeError::Io(errno)),
        }
    }
}

fn is_structural_unsupported(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::XDEV | Errno::NOSYS | Errno::NOTSUP
    )
}
