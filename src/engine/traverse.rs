//! FD-relative tree traversal.
//!
//! This module deliberately exposes only single-component operations.  A
//! caller supplies an already-open directory descriptor and a byte string for
//! one child name; no operation below reconstructs a path or performs a
//! multi-component lookup.  Directory entries are advisory and are always
//! re-statted before an object is opened.

use std::ffi::{CStr, CString};
use std::io;
use std::io::ErrorKind;
use std::ops::Deref;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, fstat, openat, statat};

#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, StatxFlags, openat2, statx};
#[cfg(target_os = "linux")]
use rustix::io::Errno;

/// Whether traversal may cross a mount boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub(crate) enum MountPolicy {
    /// Reject a child on another mount instance.
    #[default]
    StayOnMount,
    /// Permit crossing mount instances.  This is the implementation of the
    /// explicit `--cross-file-systems` mode; callers should not infer it from
    /// a device number.
    CrossFilesystems,
}

impl MountPolicy {
    fn enforces_boundary(self) -> bool {
        matches!(self, Self::StayOnMount)
    }
}

/// The kind of an entry, obtained from a trusted `statat`/`fstat` result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    fn from_file_type(file_type: FileType) -> Self {
        match file_type {
            FileType::RegularFile => Self::Regular,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// A timestamp represented in the native filesystem clock domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Timestamp {
    pub(crate) seconds: i64,
    pub(crate) nanoseconds: i64,
}

/// The mount identity associated with a stat result.
///
/// Linux mount IDs distinguish bind mounts that share `st_dev`.  On platforms
/// without an equivalent FD-relative identity, `device` is the conservative
/// volume identity available from `stat`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MountIdentity {
    pub(crate) device: u64,
    #[cfg(target_os = "linux")]
    pub(crate) mount_id: Option<u64>,
}

/// A stable identity and mutation stamp for one filesystem object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) kind: EntryKind,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) mtime: Timestamp,
    pub(crate) ctime: Timestamp,
    pub(crate) mount: MountIdentity,
}

impl FileStamp {
    pub(crate) fn same_object(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.kind == other.kind
    }
}

/// One immediate directory entry.  `kind` and `inode` are advisory values
/// from the directory stream; callers must use [`stat_child`] before acting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryEntry {
    pub(crate) name: Vec<u8>,
    pub(crate) kind: EntryKind,
    pub(crate) inode: u64,
}

/// An owned descriptor proven to refer to a directory at open time.
#[derive(Debug)]
pub(crate) struct DirectoryFd {
    fd: OwnedFd,
    stamp: FileStamp,
}

impl AsFd for DirectoryFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Deref for DirectoryFd {
    type Target = OwnedFd;

    fn deref(&self) -> &Self::Target {
        &self.fd
    }
}

impl DirectoryFd {
    /// Takes ownership of an already-open descriptor and verifies that it is
    /// a directory.  This is useful for roots established by the path layer.
    pub(crate) fn from_owned(fd: OwnedFd) -> io::Result<Self> {
        let stamp = stamp_fd(&fd)?;
        if stamp.kind != EntryKind::Directory {
            return Err(io::Error::new(
                ErrorKind::NotADirectory,
                "descriptor does not refer to a directory",
            ));
        }
        Ok(Self { fd, stamp })
    }

    pub(crate) fn stamp(&self) -> FileStamp {
        self.stamp
    }
}

/// A regular file opened after no-follow and identity checks.
#[derive(Debug)]
pub(crate) struct OpenedFile {
    fd: OwnedFd,
    stamp: FileStamp,
}

impl AsFd for OpenedFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl OpenedFile {
    pub(crate) fn stamp(&self) -> FileStamp {
        self.stamp
    }
}

/// Convert one component into a C string without permitting path traversal.
fn component(name: &[u8]) -> io::Result<CString> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "child name must be one non-empty path component",
        ));
    }
    CString::new(name).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "child name contains an embedded NUL byte",
        )
    })
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    error.into()
}

fn stamp_from_stat(stat: &Stat, mount: MountIdentity) -> FileStamp {
    FileStamp {
        device: stat.st_dev as u64,
        inode: stat.st_ino,
        kind: EntryKind::from_file_type(FileType::from_raw_mode(stat.st_mode)),
        mode: stat.st_mode as u32,
        size: stat.st_size.max(0) as u64,
        mtime: Timestamp {
            seconds: stat.st_mtime,
            nanoseconds: stat.st_mtime_nsec,
        },
        ctime: Timestamp {
            seconds: stat.st_ctime,
            nanoseconds: stat.st_ctime_nsec,
        },
        mount,
    }
}

#[cfg(target_os = "linux")]
fn mount_id_at<P: AsFd>(parent: P, name: &CStr) -> io::Result<Option<u64>> {
    match statx(parent, name, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID) {
        Ok(result) if result.stx_mask.contains(StatxFlags::MNT_ID) => Ok(Some(result.stx_mnt_id)),
        Ok(_) => Ok(None),
        Err(Errno::NOSYS) => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

fn mount_identity_for_stat<P: AsFd>(
    parent: P,
    name: &CStr,
    stat: &Stat,
) -> io::Result<MountIdentity> {
    #[cfg(not(target_os = "linux"))]
    let _ = (parent, name);
    Ok(MountIdentity {
        device: stat.st_dev as u64,
        #[cfg(target_os = "linux")]
        mount_id: mount_id_at(parent, name)?,
    })
}

fn mount_identity_for_fd<P: AsFd>(fd: P, stat: &Stat) -> io::Result<MountIdentity> {
    #[cfg(not(target_os = "linux"))]
    let _ = fd;
    #[cfg(target_os = "linux")]
    let dot = CString::new(".").expect("literal has no NUL");
    Ok(MountIdentity {
        device: stat.st_dev as u64,
        #[cfg(target_os = "linux")]
        mount_id: mount_id_at(fd, &dot)?,
    })
}

fn mount_matches(parent: MountIdentity, child: MountIdentity) -> Option<bool> {
    if parent.device != child.device {
        return Some(false);
    }
    #[cfg(target_os = "linux")]
    {
        match (parent.mount_id, child.mount_id) {
            (Some(parent), Some(child)) => Some(parent == child),
            _ => None,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(true)
    }
}

fn mount_boundary_error() -> io::Error {
    io::Error::other("mount boundary encountered; use --cross-file-systems to traverse")
}

fn mount_identity_unavailable() -> io::Error {
    io::Error::other("cannot determine mount identity safely on this Linux system")
}

fn enforce_observed_mount(
    policy: MountPolicy,
    parent: MountIdentity,
    child: MountIdentity,
) -> io::Result<()> {
    if !policy.enforces_boundary() {
        return Ok(());
    }
    match mount_matches(parent, child) {
        Some(true) => Ok(()),
        Some(false) => Err(mount_boundary_error()),
        None => Err(mount_identity_unavailable()),
    }
}

/// Obtain a no-follow stat for one child name.  This is the only metadata
/// lookup needed to classify an entry; directory stream type bits are not
/// treated as identity.
pub(crate) fn stat_child<P: AsFd>(
    parent: P,
    name: &[u8],
    policy: MountPolicy,
) -> io::Result<FileStamp> {
    let name = component(name)?;
    let stat = statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
    let parent_stat = fstat(&parent).map_err(io_error)?;
    let parent_mount = mount_identity_for_fd(&parent, &parent_stat)?;
    let mount = mount_identity_for_stat(&parent, &name, &stat)?;
    enforce_observed_mount(policy, parent_mount, mount)?;
    Ok(stamp_from_stat(&stat, mount))
}

/// Stat an already-open object.  The descriptor, rather than a pathname, is
/// the source of identity and metadata.
pub(crate) fn stamp_fd<P: AsFd>(fd: P) -> io::Result<FileStamp> {
    let stat = fstat(&fd).map_err(io_error)?;
    let mount = mount_identity_for_fd(&fd, &stat)?;
    Ok(stamp_from_stat(&stat, mount))
}

/// A bounded stream of immediate children from an already-open directory
/// descriptor.
///
/// The stream owns an independent descriptor for the supplied directory, so
/// reading it does not change the caller's directory offset or ownership. At most one
/// [`DirectoryEntry`] (and its name bytes) is live at a time.  Names are
/// returned as raw bytes and no child is opened or followed by this type.
#[derive(Debug)]
pub(crate) struct DirectoryEntries {
    stream: crate::platform::directory::DirectoryStream,
}

impl DirectoryEntries {
    /// Open a stream over the immediate children of `dir`.
    pub(crate) fn open<P: AsFd>(dir: P) -> io::Result<Self> {
        crate::platform::directory::DirectoryStream::open(dir).map(|stream| Self { stream })
    }

    /// Read the next immediate child.
    ///
    /// A `None` result means end-of-directory.  If the directory stream
    /// reports an error, it is returned and the stream becomes exhausted;
    /// callers therefore cannot accidentally continue after a partial
    /// enumeration.
    pub(crate) fn next_entry(&mut self) -> io::Result<Option<DirectoryEntry>> {
        self.stream.next_record().map(|entry| {
            entry.map(|entry| {
                let kind = match entry.kind {
                    value if value == libc::DT_REG => EntryKind::Regular,
                    value if value == libc::DT_DIR => EntryKind::Directory,
                    value if value == libc::DT_LNK => EntryKind::Symlink,
                    _ => EntryKind::Other,
                };
                DirectoryEntry {
                    name: entry.name,
                    kind,
                    inode: entry.inode,
                }
            })
        })
    }
}

impl Iterator for DirectoryEntries {
    type Item = io::Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_entry() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

/// Visit immediate children from an already-open directory without retaining
/// the directory's entries in memory.  The callback may stop the traversal by
/// returning an error; stream errors are propagated unchanged.
#[cfg(test)]
pub(crate) fn for_each_entry<P: AsFd, F>(dir: P, mut visit: F) -> io::Result<()>
where
    F: FnMut(DirectoryEntry) -> io::Result<()>,
{
    let mut entries = DirectoryEntries::open(dir)?;
    while let Some(entry) = entries.next_entry()? {
        visit(entry)?;
    }
    Ok(())
}

/// Read a symlink target without following the symlink itself.
pub(crate) fn read_symlink<P: AsFd>(parent: P, name: &[u8]) -> io::Result<Vec<u8>> {
    let name = component(name)?;
    rustix::fs::readlinkat(&parent, &name, Vec::<u8>::new())
        .map(|target| target.to_bytes().to_vec())
        .map_err(io_error)
}

#[cfg(target_os = "linux")]
fn open_one<P: AsFd>(
    parent: &P,
    name: &CStr,
    flags: OFlags,
    policy: MountPolicy,
) -> io::Result<(OwnedFd, bool)> {
    if policy.enforces_boundary() {
        match openat2(parent, name, flags, Mode::empty(), ResolveFlags::NO_XDEV) {
            Ok(fd) => return Ok((fd, true)),
            // Linux kernels before openat2 (and seccomp profiles which do not
            // expose it as a syscall) report one of these two errors.  The
            // caller will use statx mount IDs as the safe fallback.
            Err(Errno::NOSYS | Errno::INVAL) => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    openat(parent, name, flags, Mode::empty())
        .map(|fd| (fd, false))
        .map_err(io_error)
}

#[cfg(not(target_os = "linux"))]
fn open_one<P: AsFd>(
    parent: &P,
    name: &CStr,
    flags: OFlags,
    _policy: MountPolicy,
) -> io::Result<(OwnedFd, bool)> {
    openat(parent, name, flags, Mode::empty())
        .map(|fd| (fd, false))
        .map_err(io_error)
}

fn open_checked<P: AsFd>(
    parent: P,
    name: &[u8],
    expected: EntryKind,
    policy: MountPolicy,
) -> io::Result<(OwnedFd, FileStamp)> {
    let name_c = component(name)?;
    // `statat` is intentionally repeated here instead of trusting a caller's
    // earlier observation.  This closes the ordinary lookup/open replacement
    // window as far as POSIX permits.
    let observed_stat = statat(&parent, &name_c, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
    let parent_stat = fstat(&parent).map_err(io_error)?;
    let parent_mount = mount_identity_for_fd(&parent, &parent_stat)?;
    let observed_mount = mount_identity_for_stat(&parent, &name_c, &observed_stat)?;
    let observed = stamp_from_stat(&observed_stat, observed_mount);
    if observed.kind != expected {
        return Err(io::Error::new(
            match expected {
                EntryKind::Directory => ErrorKind::NotADirectory,
                EntryKind::Regular => ErrorKind::InvalidInput,
                _ => ErrorKind::InvalidInput,
            },
            "entry changed type or is not the requested object kind",
        ));
    }
    // On Linux, openat2's RESOLVE_NO_XDEV is authoritative when available.
    // Otherwise mount IDs (or the macOS device identity) are checked both
    // before and after opening.
    let flags = match expected {
        EntryKind::Directory => {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        }
        EntryKind::Regular => OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        _ => unreachable!("open_checked only accepts regular files/directories"),
    };
    let (fd, resolution_enforced) = open_one(&parent, &name_c, flags, policy)?;
    let opened = stamp_fd(&fd)?;
    if !opened.same_object(observed) {
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "entry changed while it was being opened",
        ));
    }
    if policy.enforces_boundary() {
        match mount_matches(parent_mount, opened.mount) {
            Some(true) => {}
            Some(false) => return Err(mount_boundary_error()),
            None if resolution_enforced => {}
            None => return Err(mount_identity_unavailable()),
        }
    }
    Ok((fd, opened))
}

/// Open a child directory with `O_DIRECTORY|O_NOFOLLOW`, then revalidate its
/// identity against the no-follow observation made immediately beforehand.
pub(crate) fn open_child_directory<P: AsFd>(
    parent: P,
    name: &[u8],
    policy: MountPolicy,
) -> io::Result<DirectoryFd> {
    let (fd, stamp) = open_checked(parent, name, EntryKind::Directory, policy)?;
    Ok(DirectoryFd { fd, stamp })
}

/// Open a planned directory child and prove it is still the same object that
/// the caller inspected before making a traversal or deletion decision.
///
/// [`open_child_directory`] already closes the lookup/open race against a
/// fresh observation. This variant also closes the longer planning-to-open
/// interval, so a same-type pathname replacement cannot become an authority
/// for recursive work.
pub(crate) fn open_planned_child_directory<P: AsFd>(
    parent: P,
    name: &[u8],
    expected: FileStamp,
    policy: MountPolicy,
) -> io::Result<DirectoryFd> {
    if expected.kind != EntryKind::Directory {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "planned entry is not a directory",
        ));
    }
    let directory = open_child_directory(parent, name, policy)?;
    if !directory.stamp().same_object(expected) {
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "directory changed after it was planned",
        ));
    }
    Ok(directory)
}

/// Recover a trusted directory's parent through its held descriptor.
///
/// This is used only to unwind a single-child traversal chain after the child
/// has completed. `..` is a single kernel-resolved component from an already
/// trusted directory FD; callers must still verify the returned identity
/// against the parent stamp captured before descent.
pub(crate) fn open_parent_directory(directory: &DirectoryFd) -> io::Result<DirectoryFd> {
    let fd = openat(
        directory,
        "..",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_error)?;
    DirectoryFd::from_owned(fd)
}

/// Open an independent descriptor for the same verified directory object.
///
/// This is used only by APIs whose caller retains a borrowed directory
/// capability while the implementation needs ownership to close it during a
/// deep walk. The caller still validates the returned identity before any
/// mutation is authorized.
#[cfg(test)]
pub(crate) fn reopen_directory(directory: &DirectoryFd) -> io::Result<DirectoryFd> {
    let fd = openat(
        directory,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_error)?;
    DirectoryFd::from_owned(fd)
}

/// Open a child regular file with `O_NOFOLLOW`, then revalidate its identity.
pub(crate) fn open_child_regular_file<P: AsFd>(
    parent: P,
    name: &[u8],
    policy: MountPolicy,
) -> io::Result<OpenedFile> {
    let (fd, stamp) = open_checked(parent, name, EntryKind::Regular, policy)?;
    Ok(OpenedFile { fd, stamp })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::ffi::OsString;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_root() -> (PathBuf, OwnedFd) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-traverse-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test root");
        let fd = openat(
            rustix::fs::CWD,
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open test root");
        (path, fd)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enumeration_stream_is_byte_safe_and_bounded() {
        let (path, fd) = test_root();
        let invalid = OsString::from_vec(vec![b'a', 0x80, b'b']);
        fs::File::create(path.join(&invalid)).expect("create invalid name");
        fs::File::create(path.join("plain")).expect("create plain name");
        let mut entries = Vec::new();
        for_each_entry(&fd, |entry| {
            entries.push(entry.name);
            Ok(())
        })
        .expect("enumerate");
        entries.sort_unstable();
        assert_eq!(entries, vec![b"plain".to_vec(), vec![b'a', 0x80, b'b']]);
        fs::remove_dir_all(path).expect("remove test root");
    }

    #[test]
    fn child_open_rejects_symlink_for_regular_file_and_directory() {
        let (path, fd) = test_root();
        fs::File::create(path.join("target")).expect("create target");
        std::os::unix::fs::symlink("target", path.join("file-link")).expect("link");
        fs::create_dir(path.join("dir-target")).expect("create dir target");
        std::os::unix::fs::symlink("dir-target", path.join("dir-link")).expect("dir link");
        assert!(open_child_regular_file(&fd, b"file-link", MountPolicy::CrossFilesystems).is_err());
        assert!(open_child_directory(&fd, b"dir-link", MountPolicy::CrossFilesystems).is_err());
        fs::remove_dir_all(path).expect("remove test root");
    }

    #[test]
    fn planned_directory_open_rejects_a_same_type_replacement() {
        let (path, fd) = test_root();
        fs::create_dir(path.join("planned")).expect("create planned directory");
        let planned =
            stat_child(&fd, b"planned", MountPolicy::CrossFilesystems).expect("plan directory");
        fs::rename(path.join("planned"), path.join("old")).expect("move planned directory");
        fs::create_dir(path.join("planned")).expect("replace planned directory");

        let error =
            open_planned_child_directory(&fd, b"planned", planned, MountPolicy::CrossFilesystems)
                .expect_err("same-type replacement must not become a traversal root");
        assert_eq!(error.kind(), ErrorKind::WouldBlock);
        fs::remove_dir_all(path).expect("remove test root");
    }

    #[test]
    fn invalid_component_cannot_escape_parent() {
        let (path, fd) = test_root();
        assert!(for_each_entry(&fd, |_| Ok(())).is_ok());
        assert!(stat_child(&fd, b"../outside", MountPolicy::CrossFilesystems).is_err());
        fs::remove_dir_all(path).expect("remove test root");
    }
}
