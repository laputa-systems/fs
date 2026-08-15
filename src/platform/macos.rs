//! macOS filesystem primitives.

use std::ffi::CStr;

use rustix::fd::AsFd;
use rustix::io;

/// APFS (and any other Darwin filesystem implementing the API) clone into a
/// destination-directory-relative temporary name.  The destination name is
/// created atomically by the kernel and must not already exist.
pub(crate) fn try_clone_to_dir<SrcFd: AsFd, DstFd: AsFd>(
    src: SrcFd,
    dst_dir: DstFd,
    dst_name: &CStr,
) -> io::Result<()> {
    rustix::fs::fclonefileat(src, dst_dir, dst_name, rustix::fs::CloneFlags::empty())
}
