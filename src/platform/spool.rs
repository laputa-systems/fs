//! Anonymous, bounded-memory temporary storage for traversal work.
//!
//! A directory walker cannot portably close a `DIR *`, recurse, and later
//! resume the old enumeration position: POSIX directory cookies are tied to
//! the original stream.  Materializing every child in RAM is not acceptable
//! for a directory with millions of entries either.  This small primitive
//! creates an already-unlinked file for per-directory work records, so callers
//! can stream records to disk and read them back without leaving a sidecar in
//! either selected tree.

use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, RawFd};

/// Create an anonymous temporary file which is removed automatically when the
/// returned descriptor closes.
///
/// `tmpfile(3)` performs the create-and-unlink sequence inside libc.  The
/// returned `FILE *` owns its descriptor, so duplicate that descriptor before
/// closing the C stream and transferring the duplicate to Rust's `File`.
pub(crate) fn anonymous_file() -> io::Result<File> {
    // SAFETY: `tmpfile` has no Rust-visible arguments and returns either a
    // valid owned stream or null with errno set.
    let stream = unsafe { libc::tmpfile() };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `stream` is non-null and remains owned by this function until
    // the `fclose` below. `fileno` borrows its live descriptor.
    let source = unsafe { libc::fileno(stream) };
    if source < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: this is the unique owned stream from `tmpfile`.
        unsafe { libc::fclose(stream) };
        return Err(error);
    }

    // SAFETY: `source` is live until `fclose`; `dup` returns a distinct owned
    // descriptor on success.
    let duplicate = unsafe { libc::dup(source) };
    if duplicate < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: this is the unique owned stream from `tmpfile`.
        unsafe { libc::fclose(stream) };
        return Err(error);
    }
    // SAFETY: `duplicate` is a live descriptor owned here. Setting close on
    // exec keeps this internal spool out of any child process.
    if unsafe { libc::fcntl(duplicate, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: both resources are uniquely owned on this error path.
        unsafe {
            libc::close(duplicate);
            libc::fclose(stream);
        }
        return Err(error);
    }
    // SAFETY: closing the C stream releases only `source`; `duplicate`
    // remains independently owned.
    unsafe { libc::fclose(stream) };

    // SAFETY: `duplicate` is an owned descriptor and this conversion transfers
    // its unique ownership to the returned `File`.
    Ok(unsafe { File::from_raw_fd(duplicate as RawFd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn anonymous_file_round_trips_without_a_named_sidecar() {
        let mut file = anonymous_file().expect("create anonymous spool");
        file.write_all(b"directory work").expect("write spool");
        file.seek(SeekFrom::Start(0)).expect("rewind spool");
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).expect("read spool");
        assert_eq!(contents, b"directory work");
    }
}
