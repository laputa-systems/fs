//! Atomic publication of regular files and symbolic links.
//!
//! The mutation boundary in the copy engine is deliberately small: callers
//! hand this module an already-open source object and an already-open
//! destination parent directory.  The final destination name is never opened
//! for writing.  Data is copied into an exclusively-created sibling temporary
//! object, checked, and then published with one `renameat` call.
//!
//! Names in this module are single directory-entry names represented as byte
//! slices.  This is intentional: a Unix filename is not necessarily UTF-8 and
//! converting it to a `String` would make the safety boundary lossy.  The
//! `rustix` path APIs reject embedded NUL bytes; we additionally reject `/`,
//! empty names, and `.`/`..` so a caller cannot accidentally pass a path where
//! a component is required.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::ffi::CString;

use rustix::fd::AsFd;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, SeekFrom, Stat, Timestamps};
use rustix::io::Errno;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// Keep the worker-owned buffered fallback in the documented benchmark range:
// 256 KiB is large enough for modern local filesystems without making `-j 8`
// consume an unreasonable amount of resident memory.
const COPY_BUFFER_SIZE: usize = 256 * 1024;

thread_local! {
    /// The buffered fallback runs on the calling worker. Keeping its storage
    /// thread-local bounds memory by the worker count and avoids a fresh
    /// allocation for every ordinary file copy.
    static COPY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; COPY_BUFFER_SIZE]);
    #[cfg(test)]
    static FAIL_NEXT_TEMPORARY_STAMP: Cell<bool> = const { Cell::new(false) };
}

/// A normalized timestamp used in the operation's mutation and race checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Timestamp {
    /// Seconds since the Unix epoch.
    pub(crate) seconds: i64,
    /// Nanoseconds within `seconds`.
    pub(crate) nanoseconds: i64,
}

/// The identity and mutation-relevant metadata of one filesystem object.
///
/// `dev`, `ino`, and `file_type` are the identity.  The remaining fields are
/// included when checking that a source or planned destination was not
/// changed by another process.  This type intentionally contains only stable,
/// platform-independent scalar values; it is therefore safe for callers to
/// retain between traversal and publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) file_type: FileType,
    pub(crate) size: i64,
    pub(crate) mode: u32,
    pub(crate) mtime: Timestamp,
    pub(crate) ctime: Timestamp,
}

impl FileStamp {
    /// Convert the traversal layer's no-follow observation into the stamp
    /// representation used by the publication boundary.  Keeping this
    /// conversion here lets planning retain one complete identity record and
    /// pass it through to the worker without accepting a later pathname stat
    /// as a new plan.
    pub(crate) fn from_traverse(stamp: crate::engine::traverse::FileStamp) -> Self {
        Self {
            dev: stamp.device,
            ino: stamp.inode,
            file_type: match stamp.kind {
                crate::engine::traverse::EntryKind::Regular => FileType::RegularFile,
                crate::engine::traverse::EntryKind::Directory => FileType::Directory,
                crate::engine::traverse::EntryKind::Symlink => FileType::Symlink,
                crate::engine::traverse::EntryKind::Other => FileType::Unknown,
            },
            size: stamp.size as i64,
            mode: stamp.mode,
            mtime: Timestamp {
                seconds: stamp.mtime.seconds,
                nanoseconds: stamp.mtime.nanoseconds,
            },
            ctime: Timestamp {
                seconds: stamp.ctime.seconds,
                nanoseconds: stamp.ctime.nanoseconds,
            },
        }
    }

    fn from_stat(stat: &Stat) -> Self {
        Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
            file_type: FileType::from_raw_mode(stat.st_mode),
            size: stat.st_size,
            mode: stat.st_mode as u32,
            mtime: Timestamp {
                seconds: stat.st_mtime,
                // `rustix::fs::Stat` mirrors the platform libc ABI.  glibc
                // exposes the sub-second fields as signed values while musl
                // exposes them as unsigned values; nanoseconds are always
                // non-negative and fit in an `i64` on both ABIs.
                nanoseconds: stat.st_mtime_nsec as i64,
            },
            ctime: Timestamp {
                seconds: stat.st_ctime,
                nanoseconds: stat.st_ctime_nsec as i64,
            },
        }
    }

    /// Returns whether this stamp identifies the same object and unchanged
    /// source data as `other`.
    pub(crate) fn source_is_stable(self, other: Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.file_type == other.file_type
            && self.size == other.size
            && self.mode == other.mode
            && self.mtime == other.mtime
            && self.ctime == other.ctime
    }

    /// Returns whether `other` is the same planned destination object.
    ///
    /// Mode is included here because a concurrent permission change is a
    /// destination mutation that a replacement must not silently clobber.
    pub(crate) fn destination_is_unchanged(self, other: Self) -> bool {
        self == other
    }

    /// Returns whether two stamps have the same object identity and type.
    pub(crate) fn same_identity(self, other: Self) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.file_type == other.file_type
    }
}

/// The destination state observed during planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DestinationExpectation {
    /// No final destination entry existed.
    Absent,
    /// The final destination entry had this stamp.
    Present(FileStamp),
}

/// Ephemeral record of filesystem pairs which rejected a structural
/// copy-on-write acceleration. The engine owns one cache per invocation;
/// nothing is persisted across command runs. On macOS this avoids repeated
/// `fclonefileat` attempts and on Linux it avoids repeated `FICLONE` ioctls.
#[derive(Debug, Default)]
pub(crate) struct CloneCapabilityCache {
    pairs: Mutex<HashMap<(u64, u64), CloneCapability>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloneCapability {
    Supported,
    Unsupported,
}

impl CloneCapabilityCache {
    pub(crate) fn unsupported_for(&self, source_device: u64, destination_device: u64) -> bool {
        matches!(
            self.pairs
                .lock()
                .expect("clone capability cache poisoned")
                .get(&(source_device, destination_device)),
            Some(CloneCapability::Unsupported)
        )
    }

    pub(crate) fn record_unsupported(&self, source_device: u64, destination_device: u64) {
        self.pairs
            .lock()
            .expect("clone capability cache poisoned")
            .insert(
                (source_device, destination_device),
                CloneCapability::Unsupported,
            );
    }

    pub(crate) fn record_supported(&self, source_device: u64, destination_device: u64) {
        self.pairs
            .lock()
            .expect("clone capability cache poisoned")
            .insert(
                (source_device, destination_device),
                CloneCapability::Supported,
            );
    }
}

/// Publication behavior selected by the execution engine.
///
/// The public CLI intentionally has no knobs for individual copy strategies.
/// This type only carries the storage-ordering promise made by `--durable`.
#[derive(Clone, Debug, Default)]
pub(crate) struct PublishOptions {
    pub(crate) durable: bool,
    pub(crate) clone_capabilities: Option<Arc<CloneCapabilityCache>>,
}

/// Errors returned by the publication primitives.
#[derive(Debug)]
pub(crate) enum CopyError {
    /// An underlying filesystem or I/O operation failed.
    Io(io::Error),
    /// A caller supplied something other than a single directory-entry name.
    InvalidName,
    /// The source object was not the kind required by the operation.
    WrongSourceType(FileType),
    /// The source changed while it was being copied or read.
    SourceChanged,
    /// The planned destination changed before publication.
    DestinationChanged,
    /// A temporary name no longer referred to the object owned by this
    /// operation.  It is intentionally left behind for inspection.
    CleanupConflict(Vec<u8>),
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "copy I/O error: {error}"),
            Self::InvalidName => f.write_str("invalid directory-entry name"),
            Self::WrongSourceType(kind) => write!(f, "source has unsupported type {kind:?}"),
            Self::SourceChanged => f.write_str("source changed during copy"),
            Self::DestinationChanged => f.write_str("destination changed during copy"),
            Self::CleanupConflict(name) => {
                write!(f, "temporary cleanup conflict for {:?}", name)
            }
        }
    }
}

impl std::error::Error for CopyError {}

fn io_error(error: Errno) -> CopyError {
    CopyError::Io(error.into())
}

fn validate_name(name: &[u8]) -> Result<(), CopyError> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        Err(CopyError::InvalidName)
    } else {
        Ok(())
    }
}

/// Capture a stamp from an already-open descriptor.
pub(crate) fn stamp_fd<Fd: AsFd>(fd: Fd) -> Result<FileStamp, CopyError> {
    fs::fstat(fd)
        .map(|stat| FileStamp::from_stat(&stat))
        .map_err(io_error)
}

/// Stamp a newly-created regular temporary. Tests can make this first stamp
/// fail to exercise the pre-guard recovery path without weakening the normal
/// descriptor identity check.
fn stamp_new_temporary<Fd: AsFd>(fd: Fd) -> Result<FileStamp, CopyError> {
    #[cfg(test)]
    if FAIL_NEXT_TEMPORARY_STAMP.with(|failure| failure.replace(false)) {
        return Err(CopyError::Io(io::Error::other(
            "injected temporary identity-stamp failure",
        )));
    }
    stamp_fd(fd)
}

#[cfg(test)]
fn fail_next_temporary_stamp_for_test() {
    FAIL_NEXT_TEMPORARY_STAMP.with(|failure| failure.set(true));
}

/// Capture a stamp without following the final path component.
pub(crate) fn stamp_at<Fd: AsFd>(parent: Fd, name: &[u8]) -> Result<Option<FileStamp>, CopyError> {
    validate_name(name)?;
    match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(FileStamp::from_stat(&stat))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/// Revalidate a final destination name against the state captured during
/// planning. Call this immediately before any visible mutation, including a
/// metadata-only update that works through an already-open descriptor.
pub(crate) fn check_destination<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: DestinationExpectation,
) -> Result<(), CopyError> {
    let actual = stamp_at(parent, name)?;
    match (expected, actual) {
        (DestinationExpectation::Absent, None) => Ok(()),
        (DestinationExpectation::Present(expected), Some(actual))
            if expected.destination_is_unchanged(actual) =>
        {
            Ok(())
        }
        _ => Err(CopyError::DestinationChanged),
    }
}

fn temp_name(prefix: &[u8]) -> Vec<u8> {
    let pid = std::process::id();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = Vec::with_capacity(prefix.len() + 32);
    name.extend_from_slice(prefix);
    name.push(b'.');
    name.extend_from_slice(pid.to_string().as_bytes());
    name.push(b'.');
    name.extend_from_slice(counter.to_string().as_bytes());
    name
}

/// Clean up an exclusively-created regular temporary before it has been
/// adopted by [`TempGuard`]. The first identity stamp may itself fail after
/// `openat(O_EXCL)` succeeded. Retry through the still-open descriptor, then
/// unlink only if the parent name still identifies that exact object.
fn cleanup_unadopted_entry<P: AsFd + ?Sized, Fd: AsFd>(
    parent: &P,
    name: &[u8],
    file: Fd,
) -> Result<(), CopyError> {
    let expected = stamp_fd(file)?;
    match stamp_at(parent, name)? {
        Some(actual) if expected.same_identity(actual) => {
            let flags = if expected.file_type == FileType::Directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            };
            fs::unlinkat(parent, name, flags).map_err(io_error)
        }
        Some(_) => Err(CopyError::CleanupConflict(name.to_vec())),
        None => Ok(()),
    }
}

/// Identity-checked owner of one unpublished sibling temporary object.
///
/// The guard never recursively follows its name.  Before cleanup it performs
/// a no-follow stat and compares the exact `(dev, ino, type)` captured after
/// creation.  If a separate process replaced the name, cleanup declines to
/// unlink it.  POSIX still permits a narrow replacement after this recheck and
/// before `unlinkat`; this is the residual namespace race documented by the
/// design, but no operation here ever follows an unverified replacement.
pub(crate) struct TempGuard<'a, P: AsFd + ?Sized> {
    parent: &'a P,
    name: Vec<u8>,
    expected: FileStamp,
    file: Option<rustix::fd::OwnedFd>,
    cloned: bool,
    published: bool,
}

impl<'a, P: AsFd + ?Sized> TempGuard<'a, P> {
    fn create_file(parent: &'a P) -> Result<Self, CopyError> {
        loop {
            let name = temp_name(b".fs.tmp");
            let flags =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let file = match fs::openat(parent, &name, flags, Mode::from_raw_mode(0o600)) {
                Ok(file) => file,
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(io_error(error)),
            };
            let expected = match stamp_new_temporary(&file) {
                Ok(expected) => expected,
                Err(primary) => match cleanup_unadopted_entry(parent, &name, &file) {
                    Ok(()) => return Err(primary),
                    Err(cleanup) => return Err(cleanup),
                },
            };
            return Ok(Self {
                parent,
                name,
                expected,
                file: Some(file),
                cloned: false,
                published: false,
            });
        }
    }

    fn create_symlink(parent: &'a P, target: &[u8]) -> Result<Self, CopyError> {
        if target.contains(&0) {
            // A NUL cannot occur in a POSIX symlink target.  Treat it as a
            // malformed source rather than allowing rustix's path conversion
            // to choose an implicit truncation policy.
            return Err(CopyError::InvalidName);
        }
        loop {
            let name = temp_name(b".fs.tmp");
            match fs::symlinkat(target, parent, &name) {
                Ok(()) => {
                    let c_name =
                        CString::new(name.clone()).expect("generated temporary name has no NUL");
                    // Hold a descriptor for the new link before recording its
                    // stamp, so an ordinary first-stamp failure takes the
                    // same identity-checked cleanup path as regular temps.
                    let identity = match crate::platform::open_created_symlink(parent, &c_name) {
                        Ok(identity) => identity,
                        // Without an object descriptor there is no safe way
                        // to distinguish our new symlink from a concurrent
                        // replacement. Retaining the private name is safer
                        // than unlinking an unverified object.
                        Err(error) => return Err(CopyError::Io(error)),
                    };
                    let expected = match stamp_new_temporary(&identity) {
                        Ok(expected) if expected.file_type == FileType::Symlink => expected,
                        Ok(_) => return Err(CopyError::CleanupConflict(name)),
                        Err(primary) => match cleanup_unadopted_entry(parent, &name, &identity) {
                            Ok(()) => return Err(primary),
                            Err(cleanup) => return Err(cleanup),
                        },
                    };
                    return Ok(Self {
                        parent,
                        name,
                        expected,
                        file: None,
                        cloned: false,
                        published: false,
                    });
                }
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
    }

    fn file(&self) -> &rustix::fd::OwnedFd {
        self.file
            .as_ref()
            .expect("regular-file temporary has an open descriptor")
    }

    #[cfg(target_os = "macos")]
    fn create_clone<S: AsFd>(source: S, parent: &'a P) -> Result<Option<Self>, CopyError> {
        loop {
            let name = temp_name(b".fs.tmp");
            let c_name = CString::new(name.clone()).expect("generated temporary name has no NUL");
            match crate::platform::try_clone_to_dir(&source, parent, c_name.as_c_str()) {
                Ok(()) => {
                    let file = fs::openat(
                        parent,
                        &name,
                        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                        Mode::empty(),
                    )
                    .map_err(io_error)?;
                    let expected = match stamp_new_temporary(&file) {
                        Ok(expected) => expected,
                        Err(primary) => match cleanup_unadopted_entry(parent, &name, &file) {
                            Ok(()) => return Err(primary),
                            Err(cleanup) => return Err(cleanup),
                        },
                    };
                    return Ok(Some(Self {
                        parent,
                        name,
                        expected,
                        file: Some(file),
                        cloned: true,
                        published: false,
                    }));
                }
                Err(crate::platform::CopyAttemptError::Unsupported) => return Ok(None),
                Err(crate::platform::CopyAttemptError::Io(Errno::EXIST)) => continue,
                Err(crate::platform::CopyAttemptError::Io(error)) => return Err(io_error(error)),
            }
        }
    }

    fn cleanup_inner(&mut self) -> Result<(), CopyError> {
        if self.published {
            return Ok(());
        }
        let actual = stamp_at(self.parent, &self.name)?;
        match actual {
            Some(actual) if self.expected.same_identity(actual) => {
                let flags = if self.expected.file_type == FileType::Directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                };
                fs::unlinkat(self.parent, &self.name, flags).map_err(io_error)?;
                self.published = true;
                Ok(())
            }
            Some(_) => Err(CopyError::CleanupConflict(self.name.clone())),
            None => {
                self.published = true;
                Ok(())
            }
        }
    }

    /// Revalidate and remove the unpublished object.  A replacement conflict
    /// is returned and the replacement is intentionally retained.
    #[cfg(test)]
    pub(crate) fn cleanup(mut self) -> Result<(), CopyError> {
        self.cleanup_inner()
    }

    /// Finish an error path and return the primary error unless temporary
    /// cleanup itself encountered an identity conflict.  The latter takes
    /// precedence because silently deleting an unverified replacement would
    /// violate the cleanup contract.
    fn abort<T>(mut self, primary: CopyError) -> Result<T, CopyError> {
        match self.cleanup_inner() {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(cleanup),
        }
    }

    fn publish_replace(
        mut self,
        destination_name: &[u8],
        expected_destination: DestinationExpectation,
        options: PublishOptions,
    ) -> Result<(), CopyError> {
        if options.durable
            && let Some(file) = self.file.as_ref()
            && let Err(error) = crate::platform::sync_file_for_durable_publish(file)
        {
            return self.abort(io_error(error));
        }
        if let Err(error) = check_destination(self.parent, destination_name, expected_destination) {
            return self.abort(error);
        }
        if let Err(error) =
            fs::renameat(self.parent, &self.name, self.parent, destination_name).map_err(io_error)
        {
            return self.abort(error);
        }
        self.published = true;
        if options.durable {
            crate::platform::sync_parent_directory(self.parent).map_err(io_error)?;
        }
        Ok(())
    }
}

impl<P: AsFd + ?Sized> Drop for TempGuard<'_, P> {
    fn drop(&mut self) {
        // Drop cannot report an error.  It still performs the same identity
        // check and leaves an unexpected replacement untouched.
        let _ = self.cleanup_inner();
    }
}

struct CopyOutcome {
    logical_bytes: u64,
    method: crate::platform::CopyMethod,
}

fn copy_data<S: AsFd, D: AsFd>(
    source: S,
    destination: D,
    clone_capabilities: Option<&CloneCapabilityCache>,
) -> Result<CopyOutcome, CopyError> {
    #[cfg(not(target_os = "linux"))]
    let _ = clone_capabilities;
    // A source FD may have been used by a caller before entering this
    // primitive.  Regular files are seekable, and a complete copy must always
    // begin at byte zero.
    fs::seek(&source, SeekFrom::Start(0)).map_err(io_error)?;

    #[cfg(target_os = "linux")]
    {
        let source_device = stamp_fd(&source)?.dev;
        let destination_device = stamp_fd(&destination)?.dev;
        let reflink_known_unsupported = clone_capabilities
            .is_some_and(|cache| cache.unsupported_for(source_device, destination_device));
        if !reflink_known_unsupported {
            match crate::platform::try_reflink(&source, &destination) {
                Ok(()) => {
                    if let Some(cache) = clone_capabilities {
                        cache.record_supported(source_device, destination_device);
                    }
                    let size = stamp_fd(&source)?.size;
                    let logical_bytes = u64::try_from(size).map_err(|_| {
                        CopyError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "source has a negative size",
                        ))
                    })?;
                    return Ok(CopyOutcome {
                        logical_bytes,
                        method: crate::platform::CopyMethod::Reflink,
                    });
                }
                Err(crate::platform::CopyAttemptError::Unsupported) => {
                    if let Some(cache) = clone_capabilities {
                        cache.record_unsupported(source_device, destination_device);
                    }
                }
                Err(crate::platform::CopyAttemptError::Io(error)) => return Err(io_error(error)),
            }
        }

        // `copy_file_range` is an optional whole-file path.  The platform
        // helper reports `Unsupported` only when no bytes were copied, so it
        // is safe to rewind and use the buffered fallback in that one case.
        fs::seek(&source, SeekFrom::Start(0)).map_err(io_error)?;
        fs::seek(&destination, SeekFrom::Start(0)).map_err(io_error)?;
        match crate::platform::try_copy_file_range(&source, &destination) {
            Ok(logical_bytes) => {
                return Ok(CopyOutcome {
                    logical_bytes,
                    method: crate::platform::CopyMethod::CopyFileRange,
                });
            }
            Err(crate::platform::CopyAttemptError::Unsupported) => {
                fs::seek(&source, SeekFrom::Start(0)).map_err(io_error)?;
                fs::seek(&destination, SeekFrom::Start(0)).map_err(io_error)?;
            }
            Err(crate::platform::CopyAttemptError::Io(error)) => return Err(io_error(error)),
        }
    }

    let copied = COPY_BUFFER.with(|buffer| {
        crate::platform::buffered_copy(&source, &destination, &mut buffer.borrow_mut())
            .map_err(io_error)
    })?;
    Ok(CopyOutcome {
        logical_bytes: copied,
        method: crate::platform::CopyMethod::Buffered,
    })
}

fn apply_metadata<S: AsFd, D: AsFd>(
    source: S,
    destination: D,
    stamp: FileStamp,
) -> Result<(), CopyError> {
    fs::fchmod(&destination, Mode::from_raw_mode(stamp.mode as _)).map_err(io_error)?;
    let times = Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: fs::UTIME_OMIT,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: stamp.mtime.seconds,
            tv_nsec: stamp.mtime.nanoseconds as _,
        },
    };
    fs::futimens(&destination, &times).map_err(io_error)?;
    // `metadata` owns the bounded, initialized xattr buffers.  Keeping one
    // implementation avoids a subtle but serious bug where a spare-capacity
    // buffer can be returned with length zero even though the kernel wrote
    // names into it.
    crate::metadata::propagate_xattrs(source, destination)
        .map(|_| ())
        .map_err(CopyError::Io)
}

/// Result of publishing one regular file or symlink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Publication {
    /// Number of bytes copied.  Symlink publications report zero.
    pub(crate) bytes_copied: u64,
    /// Data path used to produce the published object. `bytes_copied` is the
    /// logical byte count even for a copy-on-write clone.
    pub(crate) method: crate::platform::CopyMethod,
    /// Source stamp captured before the copy.  This is useful to callers that
    /// retain traversal metadata for diagnostics or accounting.
    pub(crate) source: FileStamp,
}

/// Copy a regular source FD into a temporary sibling and atomically publish it.
///
/// `source` must be an already-open regular-file FD (opened with no-follow
/// semantics by the root/traversal layer).  `destination_parent` is the held
/// parent directory FD and `destination_name` is one raw name component.  The
/// destination expectation is checked immediately before `renameat`; a
/// mismatch aborts and identity-checks cleanup of the temporary file.
#[cfg(test)]
pub(crate) fn publish_regular_file<S: AsFd, P: AsFd + ?Sized>(
    source: S,
    destination_parent: &P,
    destination_name: &[u8],
    expected_destination: DestinationExpectation,
) -> Result<Publication, CopyError> {
    publish_regular_file_with_options(
        source,
        destination_parent,
        destination_name,
        expected_destination,
        PublishOptions::default(),
    )
}

/// Same as [`publish_regular_file`], with the invocation's durability
/// contract.  The separate entry point keeps ordinary callers explicit about
/// the fact that `fsync` is not part of default atomic publication.
#[cfg(test)]
pub(crate) fn publish_regular_file_with_options<S: AsFd, P: AsFd + ?Sized>(
    source: S,
    destination_parent: &P,
    destination_name: &[u8],
    expected_destination: DestinationExpectation,
    options: PublishOptions,
) -> Result<Publication, CopyError> {
    publish_regular_file_checked(
        source,
        destination_parent,
        destination_name,
        None,
        expected_destination,
        options,
    )
}

/// Variant of [`publish_regular_file`] for callers that already captured a
/// source stamp during planning.  The descriptor is re-stamped before any
/// bytes are published; a mismatch aborts without touching the destination.
pub(crate) fn publish_regular_file_checked<S: AsFd, P: AsFd + ?Sized>(
    source: S,
    destination_parent: &P,
    destination_name: &[u8],
    expected_source: Option<FileStamp>,
    expected_destination: DestinationExpectation,
    options: PublishOptions,
) -> Result<Publication, CopyError> {
    validate_name(destination_name)?;
    let source_before = stamp_fd(&source)?;
    if source_before.file_type != FileType::RegularFile {
        return Err(CopyError::WrongSourceType(source_before.file_type));
    }
    if let Some(expected_source) = expected_source
        && !expected_source.source_is_stable(source_before)
    {
        return Err(CopyError::SourceChanged);
    }

    #[cfg(target_os = "macos")]
    let temporary = {
        let destination_device = stamp_fd(destination_parent)?.dev;
        let cached_unsupported = options
            .clone_capabilities
            .as_ref()
            .is_some_and(|cache| cache.unsupported_for(source_before.dev, destination_device));
        if cached_unsupported {
            TempGuard::create_file(destination_parent)?
        } else {
            match TempGuard::create_clone(&source, destination_parent)? {
                Some(temporary) => {
                    if let Some(cache) = options.clone_capabilities.as_ref() {
                        cache.record_supported(source_before.dev, destination_device);
                    }
                    temporary
                }
                None => {
                    if let Some(cache) = options.clone_capabilities.as_ref() {
                        cache.record_unsupported(source_before.dev, destination_device);
                    }
                    TempGuard::create_file(destination_parent)?
                }
            }
        }
    };
    #[cfg(not(target_os = "macos"))]
    let temporary = TempGuard::create_file(destination_parent)?;

    let copy_outcome = if temporary.cloned {
        let logical_bytes = u64::try_from(source_before.size).map_err(|_| {
            CopyError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "source has a negative size",
            ))
        })?;
        CopyOutcome {
            logical_bytes,
            method: crate::platform::CopyMethod::Clone,
        }
    } else {
        match copy_data(
            &source,
            temporary.file(),
            options.clone_capabilities.as_deref(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => return temporary.abort(error),
        }
    };
    if let Err(error) = apply_metadata(&source, temporary.file(), source_before) {
        return temporary.abort(error);
    }

    let source_after = match stamp_fd(&source) {
        Ok(stamp) => stamp,
        Err(error) => return temporary.abort(error),
    };
    if !source_before.source_is_stable(source_after) {
        return temporary.abort(CopyError::SourceChanged);
    }

    temporary.publish_replace(destination_name, expected_destination, options)?;
    Ok(Publication {
        bytes_copied: copy_outcome.logical_bytes,
        method: copy_outcome.method,
        source: source_before,
    })
}

/// Copy a source symlink without following it and atomically publish a sibling.
///
/// The source is addressed as `(source_parent, source_name)` because opening a
/// symlink for ordinary I/O would follow it on the primary Unix targets.  The
/// raw link target is preserved byte-for-byte, including non-UTF-8 bytes.
#[cfg(test)]
pub(crate) fn publish_symlink<SP: AsFd, DP: AsFd + ?Sized>(
    source_parent: SP,
    source_name: &[u8],
    destination_parent: &DP,
    destination_name: &[u8],
    expected_destination: DestinationExpectation,
) -> Result<Publication, CopyError> {
    publish_symlink_checked(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        None,
        expected_destination,
        PublishOptions::default(),
    )
}

/// Variant of [`publish_symlink`] that also validates a source stamp captured
/// during traversal/planning.
pub(crate) fn publish_symlink_checked<SP: AsFd, DP: AsFd + ?Sized>(
    source_parent: SP,
    source_name: &[u8],
    destination_parent: &DP,
    destination_name: &[u8],
    expected_source: Option<FileStamp>,
    expected_destination: DestinationExpectation,
    options: PublishOptions,
) -> Result<Publication, CopyError> {
    validate_name(source_name)?;
    validate_name(destination_name)?;
    let source_before = stamp_at(&source_parent, source_name)?
        .ok_or_else(|| CopyError::Io(io::Error::from_raw_os_error(libc::ENOENT)))?;
    if source_before.file_type != FileType::Symlink {
        return Err(CopyError::WrongSourceType(source_before.file_type));
    }
    if let Some(expected_source) = expected_source
        && !expected_source.source_is_stable(source_before)
    {
        return Err(CopyError::SourceChanged);
    }
    let target = fs::readlinkat(&source_parent, source_name, Vec::<u8>::new()).map_err(io_error)?;
    let temporary = TempGuard::create_symlink(destination_parent, target.as_bytes())?;

    let source_after = match stamp_at(&source_parent, source_name) {
        Ok(Some(stamp)) => stamp,
        Ok(None) => return temporary.abort(CopyError::SourceChanged),
        Err(error) => return temporary.abort(error),
    };
    let target_after = match fs::readlinkat(&source_parent, source_name, Vec::<u8>::new()) {
        Ok(target) => target,
        Err(error) => return temporary.abort(io_error(error)),
    };
    if !source_before.source_is_stable(source_after) || target.as_bytes() != target_after.as_bytes()
    {
        return temporary.abort(CopyError::SourceChanged);
    }

    temporary.publish_replace(destination_name, expected_destination, options)?;
    Ok(Publication {
        bytes_copied: 0,
        method: crate::platform::CopyMethod::Buffered,
        source: source_before,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self as std_fs, File};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir() -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let test_counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-copy-test-{}-{suffix}-{test_counter}",
            std::process::id()
        ));
        std_fs::create_dir(&path).expect("create test directory");
        path
    }

    #[test]
    fn regular_file_is_published_without_exposing_partial_destination() {
        let root = unique_dir();
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        std_fs::write(&source_path, b"hello atomic world").expect("write source");
        std_fs::write(&destination_path, b"old").expect("write old destination");

        let source = File::open(&source_path).expect("open source");
        let parent = File::open(&root).expect("open parent");
        let expected = stamp_at(&parent, b"destination")
            .expect("stat destination")
            .map(DestinationExpectation::Present)
            .unwrap_or(DestinationExpectation::Absent);
        let publication = publish_regular_file(&source, &parent, b"destination", expected)
            .expect("publish regular file");
        assert_eq!(publication.bytes_copied, b"hello atomic world".len() as u64);
        assert_eq!(
            std_fs::read(&destination_path).expect("read destination"),
            b"hello atomic world"
        );
        assert!(!std_fs::read_dir(&root).expect("read root").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .as_bytes()
                .starts_with(b".fs.tmp.")
        }));
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn source_change_prevents_publication() {
        let root = unique_dir();
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        std_fs::write(&source_path, b"source").expect("write source");
        std_fs::write(&destination_path, b"old").expect("write old destination");

        let source = File::open(&source_path).expect("open source");
        let parent = File::open(&root).expect("open parent");
        let before = stamp_at(&parent, b"destination")
            .expect("stat destination")
            .map(DestinationExpectation::Present)
            .unwrap_or(DestinationExpectation::Absent);
        // This test exercises the destination conflict branch directly; the
        // source-race branch is covered by the same pre-publication guard in
        // integration tests that can mutate while the buffered loop runs.
        std_fs::write(&destination_path, b"third party").expect("replace destination");
        let error = publish_regular_file(&source, &parent, b"destination", before)
            .expect_err("destination replacement must conflict");
        assert!(matches!(error, CopyError::DestinationChanged));
        assert_eq!(
            std_fs::read(&destination_path).expect("read destination"),
            b"third party"
        );
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_target_is_copied_as_raw_bytes() {
        let root = unique_dir();
        let source_path = root.join("source-link");
        let destination_path = root.join("destination-link");
        symlink(Path::new("target/does-not-exist"), &source_path).expect("create source link");
        let parent = File::open(&root).expect("open parent");
        let publication = publish_symlink(
            &parent,
            b"source-link",
            &parent,
            b"destination-link",
            DestinationExpectation::Absent,
        )
        .expect("publish symlink");
        assert_eq!(publication.bytes_copied, 0);
        assert_eq!(
            std_fs::read_link(destination_path).expect("read destination link"),
            Path::new("target/does-not-exist")
        );
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn replaced_temporary_is_retained_instead_of_unlinked() {
        let root = unique_dir();
        let parent = File::open(&root).expect("open parent");
        let temporary = TempGuard::create_file(&parent).expect("create temporary");
        let name = temporary.name.clone();
        let replacement = root.join(std::ffi::OsStr::from_bytes(&name));
        std_fs::remove_file(&replacement).expect("remove owned temporary");
        std_fs::write(&replacement, b"third-party replacement").expect("replace temporary");

        let error = temporary
            .cleanup()
            .expect_err("identity mismatch must retain the replacement");
        assert!(matches!(error, CopyError::CleanupConflict(_)));
        assert_eq!(
            std_fs::read(&replacement).expect("replacement survives"),
            b"third-party replacement"
        );
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn temporary_stamp_failure_cleans_the_exclusive_file_before_returning() {
        let root = unique_dir();
        let parent = File::open(&root).expect("open parent");
        fail_next_temporary_stamp_for_test();

        let error = match TempGuard::create_file(&parent) {
            Ok(_) => panic!("temporary identity stamp failure must be reported"),
            Err(error) => error,
        };
        assert!(matches!(error, CopyError::Io(_)));
        assert!(
            !std_fs::read_dir(&root).expect("read root").any(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .as_bytes()
                    .starts_with(b".fs.tmp.")
            }),
            "an exclusive temporary created before its guard must be cleaned"
        );
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_temporary_stamp_failure_cleans_the_owned_link_before_returning() {
        let root = unique_dir();
        let parent = File::open(&root).expect("open parent");
        fail_next_temporary_stamp_for_test();

        let error = match TempGuard::create_symlink(&parent, b"link target") {
            Ok(_) => panic!("temporary identity stamp failure must be reported"),
            Err(error) => error,
        };
        assert!(matches!(error, CopyError::Io(_)));
        assert!(
            !std_fs::read_dir(&root).expect("read root").any(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .as_bytes()
                    .starts_with(b".fs.tmp.")
            }),
            "an owned symlink created before its guard must be cleaned"
        );
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn durable_parent_sync_failure_reports_error_after_atomic_publication() {
        let root = unique_dir();
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        std_fs::write(&source_path, b"new durable contents").expect("write source");
        std_fs::write(&destination_path, b"old contents").expect("write destination");
        let source = File::open(&source_path).expect("open source");
        let parent = File::open(&root).expect("open parent");
        let expected = stamp_at(&parent, b"destination")
            .expect("stamp destination")
            .map(DestinationExpectation::Present)
            .unwrap_or(DestinationExpectation::Absent);
        crate::platform::fail_next_parent_directory_sync_for_test();

        let error = publish_regular_file_with_options(
            &source,
            &parent,
            b"destination",
            expected,
            PublishOptions {
                durable: true,
                clone_capabilities: None,
            },
        )
        .expect_err("post-rename durability failure must be returned");
        assert!(matches!(error, CopyError::Io(_)));
        assert_eq!(
            std_fs::read(&destination_path).expect("read published destination"),
            b"new durable contents",
            "the final name was already atomically published before its parent sync failed"
        );
        assert!(!std_fs::read_dir(&root).expect("read root").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .as_bytes()
                .starts_with(b".fs.tmp.")
        }));
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_failure_aborts_and_cleans_the_unpublished_temporary() {
        let root = unique_dir();
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        std_fs::write(&source_path, b"new contents").expect("write source");
        std_fs::write(&destination_path, b"old contents").expect("write destination");
        let source = File::open(&source_path).expect("open source");
        let parent = File::open(&root).expect("open parent");
        let expected = stamp_at(&parent, b"destination")
            .expect("stamp destination")
            .map(DestinationExpectation::Present)
            .unwrap_or(DestinationExpectation::Absent);
        crate::metadata::fail_next_xattr_propagation_for_test();

        let error = publish_regular_file(&source, &parent, b"destination", expected)
            .expect_err("xattr propagation failure must abort publication");
        assert!(matches!(error, CopyError::Io(_)));
        assert_eq!(
            std_fs::read(&destination_path).expect("read original destination"),
            b"old contents"
        );
        assert!(!std_fs::read_dir(&root).expect("read root").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .as_bytes()
                .starts_with(b".fs.tmp.")
        }));
        let _ = std_fs::remove_dir_all(root);
    }

    #[test]
    fn copy_on_write_capability_cache_is_scoped_by_filesystem_pair() {
        let cache = CloneCapabilityCache::default();
        assert!(!cache.unsupported_for(1, 2));
        cache.record_supported(1, 2);
        assert!(!cache.unsupported_for(1, 2));
        cache.record_unsupported(1, 2);
        assert!(cache.unsupported_for(1, 2));
        assert!(!cache.unsupported_for(2, 1));
    }
}
