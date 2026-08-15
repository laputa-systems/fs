//! FD-relative path establishment and component handling.
//!
//! This module is the only place where user-supplied paths are interpreted.
//! It establishes every intermediate component from a held directory FD with
//! `O_DIRECTORY|O_NOFOLLOW`; all later engine operations can therefore use a
//! retained parent FD and one final component.  No canonicalized path string
//! is produced or used for safety decisions.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{FsError, Result};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// The object kind observed by a no-follow final lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    RegularFile,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    fn from_file_type(file_type: FileType) -> Self {
        match file_type {
            FileType::RegularFile => Self::RegularFile,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// Stable identity and a small amount of no-follow metadata for an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EntryMetadata {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) kind: EntryKind,
    pub(crate) mode: u32,
    pub(crate) size: u64,
}

impl EntryMetadata {
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
            kind: EntryKind::from_file_type(FileType::from_raw_mode(stat.st_mode)),
            mode: stat.st_mode as u32,
            size: stat.st_size.max(0) as u64,
        }
    }

    pub(crate) fn same_identity(self, other: Self) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.kind == other.kind
    }
}

/// A root established from one user path.
///
/// `parent_fd` is always held open.  `leaf` is exactly one component (or `.`
/// when the path denotes the held starting directory).  `exists` distinguishes
/// a valid absent destination leaf from an existing object; `metadata` is only
/// present for an existing final object.
#[derive(Debug)]
pub(crate) struct EstablishedRoot {
    pub(crate) parent_fd: OwnedFd,
    pub(crate) leaf: OsString,
    pub(crate) metadata: Option<EntryMetadata>,
    pub(crate) input: OsString,
}

/// Short name used by the engine boundary when it does not need to distinguish
/// source and destination roots.
pub(crate) type Root = EstablishedRoot;

impl EstablishedRoot {
    pub(crate) fn parent_fd(&self) -> &OwnedFd {
        &self.parent_fd
    }

    pub(crate) fn leaf(&self) -> &OsStr {
        self.leaf.as_os_str()
    }

    pub(crate) fn metadata(&self) -> Option<EntryMetadata> {
        self.metadata
    }

    /// Open an existing directory root with no-follow semantics.  This is
    /// intentionally separate from establishment so a symlink root remains a
    /// symlink object instead of accidentally becoming a traversal root.
    pub(crate) fn open_directory(&self, operation: &str) -> Result<OwnedFd> {
        let metadata = self
            .metadata
            .ok_or_else(|| FsError::invalid_path(operation, &self.input, "root is absent"))?;
        if metadata.kind != EntryKind::Directory {
            return Err(FsError::invalid_path(
                operation,
                &self.input,
                "root is not a directory",
            ));
        }
        let fd = open_directory(&self.parent_fd, &self.leaf)
            .map_err(|error| FsError::io(operation, &self.input, error))?;
        let observed = stat_fd(&fd).map_err(|error| FsError::io(operation, &self.input, error))?;
        let observed = EntryMetadata::from_stat(&observed);
        if !metadata.same_identity(observed) {
            return Err(FsError::conflict(
                operation,
                &self.input,
                "root changed while it was being established",
            ));
        }
        Ok(fd)
    }
}

/// Establish a source root.  The final component may be a symlink, but every
/// intermediate component must already be a real directory.
pub(crate) fn establish_source(path: &OsStr) -> Result<EstablishedRoot> {
    establish(path, false, "establish source")
}

/// Establish a destination root.  Only the final destination component may be
/// absent; no parent directory is ever created implicitly.
pub(crate) fn establish_destination(path: &OsStr) -> Result<EstablishedRoot> {
    establish(path, true, "establish destination")
}

fn establish(path: &OsStr, allow_absent_final: bool, operation: &str) -> Result<EstablishedRoot> {
    if path.is_empty() {
        return Err(FsError::invalid_path(operation, path, "path is empty"));
    }
    #[cfg(unix)]
    if std::os::unix::ffi::OsStrExt::as_bytes(path).contains(&0) {
        return Err(FsError::invalid_path(operation, path, "path contains NUL"));
    }

    let parsed = Path::new(path);
    let components: Vec<Component<'_>> = parsed.components().collect();
    if components.is_empty() {
        return Err(FsError::invalid_path(operation, path, "path is empty"));
    }

    let absolute = matches!(components.first(), Some(Component::RootDir));
    let mut parent = if absolute {
        open_start_directory(true).map_err(|error| FsError::io(operation, path, error))?
    } else {
        open_start_directory(false).map_err(|error| FsError::io(operation, path, error))?
    };

    let mut names = components.iter().filter_map(|component| match component {
        Component::RootDir | Component::Prefix(_) => None,
        Component::CurDir => Some(OsString::from(".")),
        Component::ParentDir => Some(OsString::from("..")),
        Component::Normal(name) => Some(name.to_os_string()),
    });
    let mut all_names = names.by_ref().collect::<Vec<_>>();
    if all_names.is_empty() {
        // `/` (and equivalent redundant-root spellings) denotes the starting
        // directory itself.  `.` is a single safe final component.
        all_names.push(OsString::from("."));
    }
    let leaf = all_names.pop().expect("non-empty root component list");

    for component in all_names {
        // `.` does not perform a lookup, so it cannot introduce a symlink.
        if component == "." {
            continue;
        }
        let next = open_directory(&parent, &component)
            .map_err(|error| intermediate_error(operation, path, error))?;
        parent = next;
    }

    let metadata = match stat_nofollow(&parent, &leaf) {
        Ok(stat) => Some(EntryMetadata::from_stat(&stat)),
        Err(error) if allow_absent_final && error == Errno::NOENT => None,
        Err(error) => return Err(FsError::io(operation, path, error)),
    };

    Ok(EstablishedRoot {
        parent_fd: parent,
        leaf,
        metadata,
        input: path.to_os_string(),
    })
}

/// Missing, non-directory, and symlink intermediate components are malformed
/// roots rather than ordinary operation failures.  Preserve permission and
/// other environmental errors as operational failures.
fn intermediate_error(operation: &str, path: &OsStr, error: Errno) -> FsError {
    match error {
        Errno::NOENT | Errno::NOTDIR | Errno::LOOP => FsError::invalid_path(
            operation,
            path,
            "an intermediate path component is missing, not a directory, or a symlink",
        ),
        error => FsError::io(operation, path, error),
    }
}

fn open_start_directory(absolute: bool) -> rustix::io::Result<OwnedFd> {
    let base = if absolute { "/" } else { "." };
    fs::openat(rustix::fs::CWD, base, DIRECTORY_FLAGS, Mode::empty())
}

fn open_directory<Fd: AsFd>(parent: Fd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
    fs::openat(
        parent,
        name,
        DIRECTORY_FLAGS | OFlags::NOFOLLOW,
        Mode::empty(),
    )
}

fn stat_nofollow<Fd: AsFd>(parent: Fd, name: &OsStr) -> rustix::io::Result<rustix::fs::Stat> {
    fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
}

fn stat_fd<Fd: AsFd>(fd: Fd) -> rustix::io::Result<rustix::fs::Stat> {
    fs::fstat(fd)
}

/// Result of comparing roots before mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Overlap {
    /// The source and destination are the exact same object.  The caller must
    /// treat this as a successful no-op.
    SameObject,
    /// One root directory contains the other.  The operation must fail before
    /// any namespace mutation.
    Nested,
    /// The roots are disjoint for purposes of traversal.
    Disjoint,
}

/// Check exact identity and directory ancestry before mutation.
///
/// For an absent destination, the retained destination parent is the only
/// possible ancestry that matters: all intermediate destination components
/// already exist by construction.  For an existing destination, both
/// directions are checked.  This function follows no symlink and never uses a
/// string-prefix test.
pub(crate) fn check_overlap(
    source: &EstablishedRoot,
    destination: &EstablishedRoot,
    operation: &str,
) -> Result<Overlap> {
    let source_metadata = source.metadata.ok_or_else(|| {
        FsError::invalid_path(operation, &source.input, "source root disappeared")
    })?;
    let destination_metadata = destination.metadata;

    if destination_metadata.is_some_and(|metadata| source_metadata.same_identity(metadata)) {
        return Ok(Overlap::SameObject);
    }
    if source_metadata.kind != EntryKind::Directory {
        return Ok(Overlap::Disjoint);
    }

    let source_fd = source.open_directory(operation)?;
    if let Some(destination_metadata) = destination_metadata {
        if destination_metadata.kind != EntryKind::Directory {
            return Ok(Overlap::Disjoint);
        }
        let destination_fd = destination.open_directory(operation)?;
        if is_ancestor(&source_fd, &destination_fd)
            .map_err(|error| FsError::io(operation, &destination.input, error))?
            || is_ancestor(&destination_fd, &source_fd)
                .map_err(|error| FsError::io(operation, &source.input, error))?
        {
            return Ok(Overlap::Nested);
        }
        return Ok(Overlap::Disjoint);
    }

    // Destination is absent: check whether its held parent lies beneath the
    // source.  If it does, creating the final leaf would put the destination
    // inside the source tree.
    if is_ancestor(&source_fd, &destination.parent_fd)
        .map_err(|error| FsError::io(operation, &destination.input, error))?
    {
        Ok(Overlap::Nested)
    } else {
        Ok(Overlap::Disjoint)
    }
}

fn is_ancestor<FdA: AsFd, FdD: AsFd>(ancestor: FdA, descendant: FdD) -> rustix::io::Result<bool> {
    let mut current = fs::openat(descendant, ".", DIRECTORY_FLAGS, Mode::empty())?;
    let ancestor_stat = EntryMetadata::from_stat(&stat_fd(&ancestor)?);

    loop {
        let current_stat = EntryMetadata::from_stat(&stat_fd(&current)?);
        if ancestor_stat.same_identity(current_stat) {
            return Ok(true);
        }
        let parent = fs::openat(
            &current,
            "..",
            DIRECTORY_FLAGS | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let parent_stat = EntryMetadata::from_stat(&stat_fd(&parent)?);
        if current_stat.same_identity(parent_stat) {
            return Ok(false);
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Keep fixtures below the checkout rather than `temp_dir()`.  On macOS,
    /// the system temporary directory is commonly reached through `/var`, an
    /// intermediate symlink.  The resolver must reject that spelling by
    /// contract, so using it in these tests would test the host layout rather
    /// than final-component behavior.
    fn fixture(label: &str) -> std::path::PathBuf {
        let nonce = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()
            .expect("current directory")
            .join(format!(".fs-path-{label}-{}-{nonce}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create fixture");
        path
    }

    #[test]
    fn final_destination_may_be_absent_but_intermediate_may_not() {
        let base = fixture("destination");
        let parent = base.join("parent");
        fs::create_dir(&parent).unwrap();

        let absent = establish_destination(parent.join("new").as_os_str()).unwrap();
        assert!(absent.metadata().is_none());
        assert_eq!(absent.leaf(), OsStr::new("new"));
        assert!(establish_destination(base.join("missing/new").as_os_str()).is_err());

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn source_final_symlink_is_observed_without_following() {
        let base = fixture("symlink");
        std::os::unix::fs::symlink("target", base.join("link")).unwrap();
        let root = establish_source(base.join("link").as_os_str()).unwrap();
        assert_eq!(root.metadata().unwrap().kind, EntryKind::Symlink);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn absent_destination_inside_source_is_rejected() {
        let base = fixture("overlap");
        fs::create_dir(base.join("src")).unwrap();
        let source = establish_source(base.join("src").as_os_str()).unwrap();
        let destination = establish_destination(base.join("src/new").as_os_str()).unwrap();
        assert_eq!(
            check_overlap(&source, &destination, "copy").unwrap(),
            Overlap::Nested
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
