//! Narrow unsafe wrapper around POSIX directory streams.
//!
//! `rustix` deliberately exposes descriptor primitives rather than a
//! directory-stream iterator. Keep the `fdopendir`/`readdir` ownership and
//! errno rules here so traversal policy never needs to handle raw pointers.

use std::ffi::CStr;
use std::io;
use std::os::fd::AsRawFd;

use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags, openat};

/// One raw immediate directory entry. `kind` is advisory; callers must still
/// use their own no-follow stat before acting on the entry.
#[derive(Debug)]
pub(crate) struct DirectoryRecord {
    pub(crate) name: Vec<u8>,
    pub(crate) kind: u8,
    pub(crate) inode: u64,
}

/// An independent, bounded stream over an already-open directory.
#[derive(Debug)]
pub(crate) struct DirectoryStream {
    stream: *mut libc::DIR,
    ended: bool,
}

impl DirectoryStream {
    /// Open a separate directory stream without changing the caller's
    /// descriptor offset or ownership.
    pub(crate) fn open<P: AsFd>(directory: P) -> io::Result<Self> {
        let independent = openat(
            directory.as_fd(),
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let raw = independent.as_raw_fd();
        // SAFETY: `independent` owns `raw`. `fdopendir` takes ownership on
        // success; forgetting the `OwnedFd` transfers that responsibility to
        // the resulting stream. The error branch below closes `raw` itself.
        std::mem::forget(independent);
        // SAFETY: `raw` is a live owned directory descriptor transferred from
        // `independent`. `fdopendir` consumes it exactly on a non-null return.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed and did not consume `raw`.
            unsafe {
                libc::close(raw);
            }
            return Err(error);
        }
        Ok(Self {
            stream,
            ended: false,
        })
    }

    /// Return the next immediate entry, retaining no entries between calls.
    pub(crate) fn next_record(&mut self) -> io::Result<Option<DirectoryRecord>> {
        if self.ended {
            return Ok(None);
        }
        loop {
            clear_errno();
            // SAFETY: `self.stream` is valid until `Drop`, and this method is
            // the sole reader of it while this call is executing.
            let raw_entry = unsafe { libc::readdir(self.stream) };
            if raw_entry.is_null() {
                self.ended = true;
                let error = io::Error::last_os_error();
                if error.raw_os_error().is_some_and(|code| code != 0) {
                    return Err(error);
                }
                return Ok(None);
            }
            // SAFETY: POSIX keeps a non-null `readdir` result valid until the
            // next operation on this stream. Copy the name before looping.
            let entry = unsafe { &*raw_entry };
            // SAFETY: POSIX guarantees `d_name` is NUL-terminated here.
            let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            return Ok(Some(DirectoryRecord {
                name: name.to_vec(),
                kind: entry.d_type as u8,
                inode: entry.d_ino as u64,
            }));
        }
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        if !self.stream.is_null() {
            // SAFETY: this is the unique stream returned by `fdopendir`, and
            // this destructor closes it at most once.
            unsafe {
                libc::closedir(self.stream);
            }
            self.stream = std::ptr::null_mut();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
fn clear_errno() {
    // `readdir` may leave errno unchanged at end-of-directory. Clear this
    // thread's errno cell so end-of-stream is not mistaken for a prior error.
    // SAFETY: each selected libc accessor returns a valid pointer to the
    // current thread's errno storage.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            *libc::__error() = 0;
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
fn clear_errno() {}
