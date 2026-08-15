//! FD-relative, no-follow destination deletion.
//!
//! This module is deliberately independent of the copy and sync planners.  It
//! owns the dangerous part of pruning: every name is checked with `fstatat`
//! before it is opened or removed, recursive descent is performed through an
//! already-open directory descriptor, and destination-only directories are
//! first moved to an exclusive private name.  There is no pathname fallback
//! which can silently turn a failed no-replace rename into an overwriting
//! rename.

use super::traverse::{self, DirectoryFd, EntryKind, FileStamp, MountPolicy};
use rustix::fd::AsFd;
#[cfg(test)]
use rustix::fd::OwnedFd;
use rustix::ffi::CString;
use rustix::fs::{self, AtFlags};
#[cfg(test)]
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use std::ffi::CStr;
use std::fmt;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
use rustix::fs::RenameFlags;

#[inline]
fn is_directory(stamp: FileStamp) -> bool {
    stamp.kind == EntryKind::Directory
}

/// Errors which preserve the distinction between an ordinary I/O failure and
/// a conservative refusal to operate on a changed namespace object.
#[derive(Debug)]
pub(crate) enum DeleteError {
    Io(Errno),
    InvalidName,
    ConcurrentChange,
    CleanupConflict,
    NoReplaceUnsupported,
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::InvalidName => f.write_str("invalid directory entry name"),
            Self::ConcurrentChange => f.write_str("filesystem object changed concurrently"),
            Self::CleanupConflict => f.write_str("temporary cleanup identity conflict"),
            Self::NoReplaceUnsupported => {
                f.write_str("filesystem has no supported no-replace rename primitive")
            }
        }
    }
}

impl std::error::Error for DeleteError {}

impl From<Errno> for DeleteError {
    fn from(error: Errno) -> Self {
        Self::Io(error)
    }
}

impl From<std::io::Error> for DeleteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(Errno::from_raw_os_error(
            error.raw_os_error().unwrap_or(libc::EIO),
        ))
    }
}

pub(crate) type DeleteResult<T> = Result<T, DeleteError>;

/// A short, byte-safe private-name generator.
///
/// Names do not contain the user basename, so a `NAME_MAX`-length destination
/// remains usable.  The caller must still use exclusive creation/rename
/// primitives; this type only allocates candidates.
#[derive(Debug)]
pub(crate) struct TempNameGenerator {
    pid: u32,
    next: u64,
}

impl TempNameGenerator {
    pub(crate) fn new(pid: u32) -> Self {
        Self { pid, next: 0 }
    }

    pub(crate) fn next(&mut self, prefix: &[u8]) -> Vec<u8> {
        let counter = self.next;
        self.next = self.next.wrapping_add(1);
        let mut name = Vec::with_capacity(prefix.len() + 1 + 10 + 1 + 20);
        name.extend_from_slice(prefix);
        name.push(b'.');
        name.extend_from_slice(self.pid.to_string().as_bytes());
        name.push(b'.');
        name.extend_from_slice(counter.to_string().as_bytes());
        name
    }
}

/// Validate and convert one path component while retaining arbitrary bytes.
///
/// A component is never accepted as a path.  In particular, embedded `/` is
/// rejected so every operation remains relative to exactly one held parent FD.
fn component(name: &[u8]) -> DeleteResult<CString> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(DeleteError::InvalidName);
    }
    CString::new(name).map_err(|_| DeleteError::InvalidName)
}

/// Capture an entry without following a final symlink.
#[cfg(test)]
pub(crate) fn stamp_at<Fd: AsFd>(parent: Fd, name: &[u8]) -> DeleteResult<FileStamp> {
    stamp_at_with_policy(parent, name, MountPolicy::StayOnMount)
}

pub(crate) fn stamp_at_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    policy: MountPolicy,
) -> DeleteResult<FileStamp> {
    component(name)?;
    Ok(traverse::stat_child(parent, name, policy)?)
}

fn stamp_at_c_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &CStr,
    policy: MountPolicy,
) -> DeleteResult<FileStamp> {
    stamp_at_with_policy(parent, name.to_bytes(), policy)
}

fn stamp_fd<Fd: AsFd>(fd: Fd) -> DeleteResult<FileStamp> {
    Ok(traverse::stamp_fd(fd)?)
}

fn verify_name_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &CStr,
    expected: FileStamp,
    policy: MountPolicy,
) -> DeleteResult<FileStamp> {
    let current = stamp_at_c_with_policy(parent, name, policy)?;
    if !current.same_object(expected) {
        return Err(DeleteError::ConcurrentChange);
    }
    Ok(current)
}

#[cfg(test)]
fn verify_name<Fd: AsFd>(parent: Fd, name: &CStr, expected: FileStamp) -> DeleteResult<FileStamp> {
    verify_name_with_policy(parent, name, expected, MountPolicy::StayOnMount)
}

/// Open a directory child after checking that the final name is a directory.
/// The returned FD, rather than the name, is then the authority for recursion.
#[cfg(test)]
pub(crate) fn open_directory<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: Option<FileStamp>,
) -> DeleteResult<(DirectoryFd, FileStamp)> {
    open_directory_with_policy(parent, name, expected, MountPolicy::StayOnMount)
}

pub(crate) fn open_directory_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: Option<FileStamp>,
    policy: MountPolicy,
) -> DeleteResult<(DirectoryFd, FileStamp)> {
    let observed = stamp_at_with_policy(&parent, name, policy)?;
    if !is_directory(observed) {
        return Err(DeleteError::ConcurrentChange);
    }
    if let Some(expected) = expected
        && !observed.same_object(expected) {
            return Err(DeleteError::ConcurrentChange);
        }
    let fd = traverse::open_child_directory(&parent, name, policy)?;
    let opened = fd.stamp();
    if !opened.same_object(observed) {
        return Err(DeleteError::ConcurrentChange);
    }
    Ok((fd, opened))
}

/// Remove a regular file, symlink, or other non-directory object only when
/// the final no-follow lookup still identifies the object the planner saw.
#[cfg(test)]
pub(crate) fn unlink_checked<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: FileStamp,
) -> DeleteResult<()> {
    unlink_checked_with_policy(parent, name, expected, MountPolicy::StayOnMount)
}

pub(crate) fn unlink_checked_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: FileStamp,
    policy: MountPolicy,
) -> DeleteResult<()> {
    if expected.kind == EntryKind::Directory {
        return Err(DeleteError::ConcurrentChange);
    }
    let name = component(name)?;
    verify_name_with_policy(&parent, name.as_c_str(), expected, policy)?;
    fs::unlinkat(parent, name.as_c_str(), AtFlags::empty())?;
    Ok(())
}

/// An optional policy hook called before every child is opened or removed.
///
/// The callback is intentionally part of the deletion API rather than a CLI
/// concern.  A sync planner can use it to reject a child whose mount identity
/// differs from the selected root.  A no-op callback (`|_| Ok(())`) retains
/// ordinary same-mount behavior.
pub(crate) type MountCheck<'a> = dyn FnMut(&FileStamp) -> DeleteResult<()> + 'a;

fn delete_contents(
    directory: &DirectoryFd,
    expected: FileStamp,
    mount_check: &mut MountCheck<'_>,
    policy: MountPolicy,
) -> DeleteResult<()> {
    let opened = stamp_fd(directory)?;
    if !opened.same_object(expected) || !is_directory(opened) {
        return Err(DeleteError::ConcurrentChange);
    }

    // Enumeration borrows only the held directory FD. Directory stream type
    // bits are advisory; every entry is restatted below.
    let entries = traverse::enumerate(directory)?;

    for entry in entries {
        let observed = match stamp_at_with_policy(directory, &entry.name, policy) {
            Ok(stamp) => stamp,
            Err(DeleteError::Io(error)) if error == Errno::NOENT => continue,
            Err(error) => return Err(error),
        };
        mount_check(&observed)?;

        if is_directory(observed) {
            let (child, child_stamp) =
                open_directory_with_policy(directory, &entry.name, Some(observed), policy)?;
            delete_contents(&child, child_stamp, mount_check, policy)?;

            // The child was emptied through its FD. Revalidate the name
            // before the final rmdir; a replacement is left untouched.
            let name = component(&entry.name)?;
            verify_name_with_policy(directory, name.as_c_str(), observed, policy)?;
            fs::unlinkat(directory, name.as_c_str(), AtFlags::REMOVEDIR)?;
        } else {
            // This includes regular files and symlinks. No object is opened
            // for execution or traversal, and no link target is followed.
            unlink_checked_with_policy(directory, &entry.name, observed, policy)?;
        }
    }

    Ok(())
}

/// Recursively delete an already-open directory's contents.
///
/// `directory` is never reacquired by pathname.  The caller is responsible for
/// removing the directory's own parent name after this succeeds.
#[cfg(test)]
pub(crate) fn remove_directory_contents(
    directory: &DirectoryFd,
    expected: FileStamp,
    mount_check: &mut MountCheck<'_>,
) -> DeleteResult<()> {
    remove_directory_contents_with_policy(
        directory,
        expected,
        mount_check,
        MountPolicy::StayOnMount,
    )
}

pub(crate) fn remove_directory_contents_with_policy(
    directory: &DirectoryFd,
    expected: FileStamp,
    mount_check: &mut MountCheck<'_>,
    policy: MountPolicy,
) -> DeleteResult<()> {
    if !is_directory(expected) {
        return Err(DeleteError::ConcurrentChange);
    }
    delete_contents(directory, expected, mount_check, policy)
}

/// The private name used when destination-only directories are taken out of
/// the mutable namespace before recursive deletion.
const REMOVE_PREFIX: &[u8] = b".fs.rm";

/// Rename a directory to an absent private sibling using a platform
/// no-replace primitive.  `renameat_with(..., NOREPLACE)` is available on the
/// supported Linux and Darwin targets.  Unsupported targets fail closed.
pub(crate) fn rename_directory_aside_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: FileStamp,
    names: &mut TempNameGenerator,
    policy: MountPolicy,
) -> DeleteResult<Vec<u8>> {
    if !is_directory(expected) {
        return Err(DeleteError::ConcurrentChange);
    }
    let source = component(name)?;
    verify_name_with_policy(&parent, source.as_c_str(), expected, policy)?;

    loop {
        let private_name = names.next(REMOVE_PREFIX);
        let private = component(&private_name)?;
        match rename_no_replace(&parent, source.as_c_str(), &parent, private.as_c_str()) {
            Ok(()) => {
                // The source inode is now owned under the private name.  Do
                // not recurse unless that ownership can still be established.
                match verify_name_with_policy(&parent, private.as_c_str(), expected, policy) {
                    Ok(_) => return Ok(private_name),
                    Err(DeleteError::Io(error)) if error == Errno::NOENT => {
                        return Err(DeleteError::CleanupConflict);
                    }
                    Err(_) => return Err(DeleteError::CleanupConflict),
                }
            }
            Err(DeleteError::Io(error)) if error == Errno::EXIST => continue,
            Err(DeleteError::Io(error))
                if error.raw_os_error() == libc::ENOTSUP
                    || error.raw_os_error() == libc::EOPNOTSUPP
                    || error.raw_os_error() == libc::EINVAL
                    || error.raw_os_error() == libc::ENOSYS =>
            {
                return Err(DeleteError::NoReplaceUnsupported);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
fn rename_no_replace<PFd: AsFd, QFd: AsFd>(
    source_parent: PFd,
    source: &CStr,
    destination_parent: QFd,
    destination: &CStr,
) -> DeleteResult<()> {
    fs::renameat_with(
        source_parent,
        source,
        destination_parent,
        destination,
        RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
fn rename_no_replace<PFd: AsFd, QFd: AsFd>(
    _source_parent: PFd,
    _source: &CStr,
    _destination_parent: QFd,
    _destination: &CStr,
) -> DeleteResult<()> {
    Err(DeleteError::NoReplaceUnsupported)
}

/// Take a destination-only directory private, recursively remove its contents,
/// and finally remove the private directory after an identity check.
///
/// If any step fails, the private name is intentionally retained.  A later
/// caller can report or inspect it; no cleanup path guesses at ownership.
#[cfg(test)]
pub(crate) fn prune_directory<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: FileStamp,
    names: &mut TempNameGenerator,
    mount_check: &mut MountCheck<'_>,
) -> DeleteResult<()> {
    prune_directory_with_policy(
        parent,
        name,
        expected,
        names,
        mount_check,
        MountPolicy::StayOnMount,
    )
}

pub(crate) fn prune_directory_with_policy<Fd: AsFd>(
    parent: Fd,
    name: &[u8],
    expected: FileStamp,
    names: &mut TempNameGenerator,
    mount_check: &mut MountCheck<'_>,
    policy: MountPolicy,
) -> DeleteResult<()> {
    let private_name = rename_directory_aside_with_policy(&parent, name, expected, names, policy)?;
    let private = component(&private_name)?;
    let directory = traverse::open_child_directory(&parent, &private_name, policy)?;
    let opened = directory.stamp();
    if !opened.same_object(expected) {
        return Err(DeleteError::CleanupConflict);
    }
    remove_directory_contents_with_policy(&directory, opened, mount_check, policy)?;

    // The directory must be empty and must still be the exact object moved
    // aside by this operation.  Never rmdir an unverified replacement.
    verify_name_with_policy(&parent, private.as_c_str(), expected, policy).map_err(|error| {
        match error {
            DeleteError::Io(Errno::NOENT) => DeleteError::CleanupConflict,
            _ => DeleteError::CleanupConflict,
        }
    })?;
    fs::unlinkat(&parent, private.as_c_str(), AtFlags::REMOVEDIR)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self as std_fs, File};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-delete-test-{}-{nonce}-{counter}",
            std::process::id()
        ));
        std_fs::create_dir(&path).expect("create test root");
        path
    }

    fn open_root(path: &std::path::Path) -> OwnedFd {
        fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open root")
    }

    fn no_mounts(_: &FileStamp) -> DeleteResult<()> {
        Ok(())
    }

    #[test]
    fn private_name_does_not_depend_on_destination_basename_length() {
        let mut names = TempNameGenerator::new(u32::MAX);
        let candidate = names.next(b".fs.rm");
        assert!(candidate.len() < 64);
        assert_eq!(&candidate[..7], b".fs.rm.");
    }

    #[test]
    fn unlink_checked_does_not_follow_symlink_and_rejects_stale_identity() {
        let root = temp_dir();
        let outside = root.with_file_name(format!("{}-outside", root.display()));
        std_fs::write(&outside, b"outside").expect("outside");
        std_fs::write(root.join("file"), b"inside").expect("file");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("link");

        let fd = open_root(&root);
        let file_stamp = stamp_at(&fd, b"file").expect("stamp");
        std_fs::remove_file(root.join("file")).expect("replace file");
        std_fs::write(root.join("file"), b"replacement").expect("replacement");
        assert!(matches!(
            unlink_checked(&fd, b"file", file_stamp),
            Err(DeleteError::ConcurrentChange)
        ));

        let link_stamp = stamp_at(&fd, b"link").expect("link stamp");
        unlink_checked(&fd, b"link", link_stamp).expect("unlink link");
        assert!(outside.exists(), "unlink must not touch link target");

        std_fs::remove_dir_all(root).expect("cleanup");
        std_fs::remove_file(outside).expect("cleanup outside");
    }

    #[test]
    fn recursive_delete_uses_open_directory_and_revalidates_children() {
        let root = temp_dir();
        let tree = root.join("tree");
        std_fs::create_dir(&tree).expect("tree");
        std_fs::create_dir(tree.join("nested")).expect("nested");
        let mut file = File::create(tree.join("nested").join("file")).expect("file");
        file.write_all(b"contents").expect("write");
        let target = root.join("outside");
        std_fs::write(&target, b"target").expect("target");
        std::os::unix::fs::symlink(&target, tree.join("link")).expect("symlink");

        let fd = open_root(&root);
        let tree_stamp = stamp_at(&fd, b"tree").expect("tree stamp");
        let (tree_fd, opened) = open_directory(&fd, b"tree", Some(tree_stamp)).expect("open");
        let mut check = no_mounts;
        remove_directory_contents(&tree_fd, opened, &mut check).expect("remove contents");
        verify_name(&fd, c"tree", tree_stamp).expect("tree still exists");
        fs::unlinkat(&fd, c"tree", AtFlags::REMOVEDIR).expect("remove tree");
        assert!(target.exists(), "symlink target must survive");

        std_fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn prune_directory_uses_short_name_and_no_replace() {
        let root = temp_dir();
        let tree = root.join("destination-only");
        std_fs::create_dir(&tree).expect("tree");
        std_fs::write(tree.join("file"), b"contents").expect("file");
        std_fs::write(root.join(".fs.rm.42.0"), b"pre-existing").expect("collision");
        let fd = open_root(&root);
        let expected = stamp_at(&fd, b"destination-only").expect("stamp");
        let mut names = TempNameGenerator::new(42);
        let mut check = no_mounts;
        prune_directory(&fd, b"destination-only", expected, &mut names, &mut check).expect("prune");
        assert!(!root.join("destination-only").exists());
        assert_eq!(
            std_fs::read(root.join(".fs.rm.42.0")).expect("collision survives"),
            b"pre-existing"
        );
        assert_eq!(
            root.read_dir().expect("read root").count(),
            1,
            "only the pre-existing collision remains"
        );
        std_fs::remove_dir_all(root).expect("cleanup");
    }
}
