//! `cp` overlay and two-phase `sync` convergence.
//!
//! This is deliberately the orchestration layer, not a second filesystem
//! abstraction.  Roots have already been established through held directory
//! descriptors by `path`; every operation below passes one component to the
//! traversal, copy, or deletion primitives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use rustix::fd::AsFd;
use rustix::fs::{self, Mode, OFlags};

use crate::cli::{CheckMode as CliCheckMode, CommandLine, Operation};
use crate::engine::{compare, copy, delete, traverse};
use crate::error::{FsError, Result};
use crate::metadata::{self, TimestampComparison};
use crate::path::{self, EntryKind as RootEntryKind, Overlap, Root};
use crate::progress::{Progress, ProgressRenderer};
use crate::workers::{
    Cancellation, SendError, WorkQueue, WorkSender, WorkerGroup, WorkerJoinError,
};

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

/// One directory child captured during the initial enumeration pass.
///
/// The records live in an anonymous temporary file rather than a `Vec`: a
/// single very wide directory must not make resident memory proportional to
/// its entry count.  Capturing the complete no-follow observations also lets
/// descent use the normal planned-child checks after the enumeration stream
/// is closed, without attempting to resume a new `DIR *` at an old position.
struct DirectoryWorkRecord {
    name: Vec<u8>,
    source: Option<traverse::FileStamp>,
    destination: Option<traverse::FileStamp>,
}

struct DirectoryChildSpool {
    writer: BufWriter<std::fs::File>,
}

struct DirectoryChildReader {
    reader: BufReader<std::fs::File>,
}

/// Per-directory child work starts in memory for the common small-directory
/// case, then spills to the anonymous spool before it becomes material. A
/// dry-run must perform no filesystem mutations at all, including creating an
/// otherwise-unlinked system temporary, so it deliberately remains in memory.
const DIRECTORY_WORK_MEMORY_LIMIT: usize = 256 * 1024;

enum DirectoryWorkStorage {
    Memory {
        records: Vec<DirectoryWorkRecord>,
        bytes: usize,
        may_spill: bool,
    },
    Spool(DirectoryChildSpool),
}

enum DirectoryWorkReader {
    Memory(std::vec::IntoIter<DirectoryWorkRecord>),
    Spool(DirectoryChildReader),
}

impl DirectoryWorkStorage {
    fn new(dry_run: bool) -> Self {
        Self::Memory {
            records: Vec::new(),
            bytes: 0,
            may_spill: !dry_run,
        }
    }

    fn push(&mut self, record: DirectoryWorkRecord) -> io::Result<()> {
        const RECORD_OVERHEAD: usize = 192;
        let record_bytes = record.name.len().saturating_add(RECORD_OVERHEAD);
        if let Self::Memory {
            records,
            bytes,
            may_spill,
        } = self
        {
            if *may_spill && bytes.saturating_add(record_bytes) > DIRECTORY_WORK_MEMORY_LIMIT {
                let mut spool = DirectoryChildSpool::new()?;
                for buffered in records.drain(..) {
                    spool.push(&buffered)?;
                }
                spool.push(&record)?;
                *self = Self::Spool(spool);
                return Ok(());
            }
            *bytes = bytes.saturating_add(record_bytes);
            records.push(record);
            return Ok(());
        }
        let Self::Spool(spool) = self else {
            unreachable!("directory work storage is memory or spool")
        };
        spool.push(&record)
    }

    fn into_reader(self) -> io::Result<DirectoryWorkReader> {
        match self {
            Self::Memory { records, .. } => Ok(DirectoryWorkReader::Memory(records.into_iter())),
            Self::Spool(spool) => spool.into_reader().map(DirectoryWorkReader::Spool),
        }
    }
}

impl DirectoryWorkReader {
    fn next(&mut self) -> io::Result<Option<DirectoryWorkRecord>> {
        match self {
            Self::Memory(records) => Ok(records.next()),
            Self::Spool(reader) => reader.next(),
        }
    }
}

impl DirectoryChildSpool {
    fn new() -> io::Result<Self> {
        Ok(Self {
            writer: BufWriter::new(crate::platform::spool::anonymous_file()?),
        })
    }

    fn push(&mut self, record: &DirectoryWorkRecord) -> io::Result<()> {
        let name_length = u32::try_from(record.name.len()).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidData,
                "directory entry name exceeds spool record limit",
            )
        })?;
        self.writer.write_all(&name_length.to_le_bytes())?;
        self.writer.write_all(&record.name)?;
        match record.source {
            Some(stamp) => {
                self.writer.write_all(&[1])?;
                write_traverse_stamp(&mut self.writer, stamp)?;
            }
            None => self.writer.write_all(&[0])?,
        }
        match record.destination {
            Some(stamp) => {
                self.writer.write_all(&[1])?;
                write_traverse_stamp(&mut self.writer, stamp)?;
            }
            None => self.writer.write_all(&[0])?,
        }
        Ok(())
    }

    fn into_reader(mut self) -> io::Result<DirectoryChildReader> {
        self.writer.flush()?;
        let mut file = self
            .writer
            .into_inner()
            .map_err(|error| error.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        Ok(DirectoryChildReader {
            reader: BufReader::new(file),
        })
    }
}

impl DirectoryChildReader {
    fn next(&mut self) -> io::Result<Option<DirectoryWorkRecord>> {
        let mut first_length_byte = [0_u8; 1];
        if self.reader.read(&mut first_length_byte)? == 0 {
            return Ok(None);
        }
        let mut remaining_length = [0_u8; 3];
        self.reader.read_exact(&mut remaining_length)?;
        let name_length = u32::from_le_bytes([
            first_length_byte[0],
            remaining_length[0],
            remaining_length[1],
            remaining_length[2],
        ]);
        // Directory APIs never return components this large. The cap turns a
        // corrupted anonymous spool into a normal error rather than a large
        // allocation if a platform I/O failure produces malformed bytes.
        const MAX_COMPONENT_BYTES: usize = 1024 * 1024;
        let name_length = usize::try_from(name_length).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidData,
                "invalid directory spool name length",
            )
        })?;
        if name_length > MAX_COMPONENT_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "directory spool name length is implausibly large",
            ));
        }
        let mut name = vec![0; name_length];
        self.reader.read_exact(&mut name)?;
        let mut source_present = [0_u8; 1];
        self.reader.read_exact(&mut source_present)?;
        let source = match source_present[0] {
            0 => None,
            1 => Some(read_traverse_stamp(&mut self.reader)?),
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid source marker in directory spool",
                ));
            }
        };
        let mut destination_present = [0_u8; 1];
        self.reader.read_exact(&mut destination_present)?;
        let destination = match destination_present[0] {
            0 => None,
            1 => Some(read_traverse_stamp(&mut self.reader)?),
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid destination marker in directory spool",
                ));
            }
        };
        Ok(Some(DirectoryWorkRecord {
            name,
            source,
            destination,
        }))
    }
}

fn write_traverse_stamp<W: Write>(writer: &mut W, stamp: traverse::FileStamp) -> io::Result<()> {
    writer.write_all(&stamp.device.to_le_bytes())?;
    writer.write_all(&stamp.inode.to_le_bytes())?;
    let kind = match stamp.kind {
        traverse::EntryKind::Regular => 0,
        traverse::EntryKind::Directory => 1,
        traverse::EntryKind::Symlink => 2,
        traverse::EntryKind::Other => 3,
    };
    writer.write_all(&[kind])?;
    writer.write_all(&stamp.mode.to_le_bytes())?;
    writer.write_all(&stamp.size.to_le_bytes())?;
    writer.write_all(&stamp.mtime.seconds.to_le_bytes())?;
    writer.write_all(&stamp.mtime.nanoseconds.to_le_bytes())?;
    writer.write_all(&stamp.ctime.seconds.to_le_bytes())?;
    writer.write_all(&stamp.ctime.nanoseconds.to_le_bytes())?;
    writer.write_all(&stamp.mount.device.to_le_bytes())?;
    #[cfg(target_os = "linux")]
    match stamp.mount.mount_id {
        Some(mount_id) => {
            writer.write_all(&[1])?;
            writer.write_all(&mount_id.to_le_bytes())?;
        }
        None => writer.write_all(&[0])?,
    }
    Ok(())
}

fn read_traverse_stamp<R: Read>(reader: &mut R) -> io::Result<traverse::FileStamp> {
    fn read_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
        let mut bytes = [0; 8];
        reader.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }
    fn read_i64<R: Read>(reader: &mut R) -> io::Result<i64> {
        let mut bytes = [0; 8];
        reader.read_exact(&mut bytes)?;
        Ok(i64::from_le_bytes(bytes))
    }
    fn read_u32<R: Read>(reader: &mut R) -> io::Result<u32> {
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    let device = read_u64(reader)?;
    let inode = read_u64(reader)?;
    let mut kind = [0_u8; 1];
    reader.read_exact(&mut kind)?;
    let kind = match kind[0] {
        0 => traverse::EntryKind::Regular,
        1 => traverse::EntryKind::Directory,
        2 => traverse::EntryKind::Symlink,
        3 => traverse::EntryKind::Other,
        _ => {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid directory spool kind",
            ));
        }
    };
    let mode = read_u32(reader)?;
    let size = read_u64(reader)?;
    let mtime = traverse::Timestamp {
        seconds: read_i64(reader)?,
        nanoseconds: read_i64(reader)?,
    };
    let ctime = traverse::Timestamp {
        seconds: read_i64(reader)?,
        nanoseconds: read_i64(reader)?,
    };
    let mount_device = read_u64(reader)?;
    #[cfg(target_os = "linux")]
    let mount_id = {
        let mut present = [0_u8; 1];
        reader.read_exact(&mut present)?;
        match present[0] {
            0 => None,
            1 => Some(read_u64(reader)?),
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid directory spool mount marker",
                ));
            }
        }
    };
    Ok(traverse::FileStamp {
        device,
        inode,
        kind,
        mode,
        size,
        mtime,
        ctime,
        mount: traverse::MountIdentity {
            device: mount_device,
            #[cfg(target_os = "linux")]
            mount_id,
        },
    })
}

thread_local! {
    /// Each worker owns one reusable BLAKE3 read buffer. This bounds memory
    /// while avoiding a megabyte allocation per compared file.
    static FILE_HASH_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; HASH_BUFFER_SIZE]);
}

#[cfg(test)]
static PRUNE_MUTATION_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);
#[cfg(test)]
static PHASE_A_MUTATION_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);
#[cfg(test)]
static FILE_WORKER_FAILURE_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);
#[cfg(test)]
static DESTINATION_CHILD_REPLACEMENT_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);

/// Inject one worker-side I/O failure for a chosen source inode. Keeping the
/// target as an inode avoids cross-test interference while exercising the
/// actual cancellation and Phase-B gate rather than a synthetic early return
/// from the directory walker.
#[cfg(test)]
fn fail_selected_file_worker_for_test(source: copy::FileStamp) -> Option<FsError> {
    let should_fail = {
        let mut target = FILE_WORKER_FAILURE_TARGET
            .lock()
            .expect("file worker failure target poisoned");
        if *target == Some((source.dev, source.ino)) {
            *target = None;
            true
        } else {
            false
        }
    };
    should_fail.then(|| {
        FsError::io(
            "copy test failure",
            OsStr::new("."),
            io::Error::other("injected worker I/O failure"),
        )
    })
}

/// Deterministically replace an already-open destination child after its
/// recursive work returns. This models a same-parent rename/create race which
/// does not replace the parent itself and therefore requires child-name
/// revalidation rather than only checking `..`.
#[cfg(test)]
fn replace_destination_child_after_phase_a_for_test(
    child: &traverse::DirectoryFd,
    name: &[u8],
) -> std::io::Result<()> {
    let stamp = child.stamp();
    let should_replace = {
        let mut target = DESTINATION_CHILD_REPLACEMENT_TARGET
            .lock()
            .expect("destination child replacement target poisoned");
        if *target == Some((stamp.device, stamp.inode)) {
            *target = None;
            true
        } else {
            false
        }
    };
    if !should_replace {
        return Ok(());
    }
    let parent = traverse::open_parent_directory(child)?;
    let parked = b".fs-test-destination-child-parked";
    fs::renameat(&parent, name, &parent, &parked[..]).map_err(std::io::Error::from)?;
    fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700)).map_err(std::io::Error::from)
}

/// Deterministic regression seam for Phase A's directory-membership guard.
/// The mutation happens through the held directory FD immediately after its
/// before-stamp, so this test cannot be made flaky by pathname timing.
#[cfg(test)]
fn mutate_source_during_phase_a_for_test(source: &traverse::DirectoryFd) -> std::io::Result<()> {
    let stamp = traverse::stamp_fd(source)?;
    let should_mutate = {
        let mut target = PHASE_A_MUTATION_TARGET
            .lock()
            .expect("Phase A mutation target poisoned");
        if *target == Some((stamp.device, stamp.inode)) {
            *target = None;
            true
        } else {
            false
        }
    };
    if should_mutate {
        let _file = fs::openat(
            source,
            &b".fs-test-source-phase-a-mutation"[..],
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
    }
    Ok(())
}

/// Deterministic regression seam for the documented live-source prune
/// boundary. Production builds contain no hook: the test asks this function
/// to mutate one specific source directory through its already-held FD after
/// the before-stamp and before destination membership is examined.
#[cfg(test)]
fn mutate_source_during_prune_for_test(source: &traverse::DirectoryFd) -> std::io::Result<()> {
    let stamp = traverse::stamp_fd(source)?;
    let should_mutate = {
        let mut target = PRUNE_MUTATION_TARGET
            .lock()
            .expect("prune mutation target poisoned");
        if *target == Some((stamp.device, stamp.inode)) {
            *target = None;
            true
        } else {
            false
        }
    };
    if should_mutate {
        let _file = fs::openat(
            source,
            &b".fs-test-source-prune-mutation"[..],
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RunOptions {
    operation: Operation,
    check: compare::CheckMode,
    mount_policy: traverse::MountPolicy,
    dry_run: bool,
    verbose: bool,
    durable: bool,
    jobs: usize,
}

impl From<&CommandLine> for RunOptions {
    fn from(command: &CommandLine) -> Self {
        Self {
            operation: command.operation,
            check: match command.options.check {
                CliCheckMode::Metadata => compare::CheckMode::Metadata,
                CliCheckMode::Hash => compare::CheckMode::Hash,
            },
            mount_policy: if command.options.cross_file_systems {
                traverse::MountPolicy::CrossFilesystems
            } else {
                traverse::MountPolicy::StayOnMount
            },
            dry_run: command.options.dry_run,
            verbose: command.options.verbose,
            durable: command.options.durable,
            jobs: command
                .options
                .jobs
                .map(|jobs| jobs.get())
                .unwrap_or_else(default_jobs),
        }
    }
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(2)
        .clamp(2, 8)
}

/// One regular-file convergence operation detached from the directory walk.
///
/// It owns descriptor capabilities rather than paths, so the worker can run
/// after the walker's stack has moved on without reopening an attacker-owned
/// namespace component.
struct FileTask {
    source: traverse::OpenedFile,
    /// Shared directory capability for every file task in one directory. A
    /// queue of sibling files must not duplicate this FD once per child.
    destination_parent: Arc<traverse::DirectoryFd>,
    /// Root-relative display path. Filesystem operations continue to use the
    /// single `name` component below.
    display_path: Vec<u8>,
    name: Vec<u8>,
    expected_source: copy::FileStamp,
    expected_destination: copy::DestinationExpectation,
    destination_exists: bool,
    options: RunOptions,
    progress: Arc<Progress>,
    timestamp_resolutions: TimestampResolutionCache,
    clone_capabilities: Arc<copy::CloneCapabilityCache>,
    /// Every worker checks this immediately before a visible mutation. A
    /// failure cannot make already-running work transactional, but it bounds
    /// post-failure publications to work which passed this final boundary
    /// before cancellation was observed.
    cancellation: Option<Cancellation>,
    completion: Option<FileCompletionTicket>,
}

/// Scoped completion accounting for the current prefix of scheduled files.
///
/// A directory finalizer waits for this counter rather than retaining every
/// descendant directory FD until the whole walk ends. Each task owns a ticket,
/// so normal completion, task failure, and queue abandonment all release the
/// accounting exactly once when the task is dropped.
#[derive(Default)]
struct FileCompletion {
    outstanding: Mutex<usize>,
    changed: Condvar,
}

impl FileCompletion {
    fn reserve(self: &Arc<Self>) -> FileCompletionTicket {
        let mut outstanding = self.outstanding.lock().expect("file completion poisoned");
        *outstanding += 1;
        FileCompletionTicket {
            completion: self.clone(),
        }
    }

    fn wait_until_drained_or_cancelled(&self, cancellation: &Cancellation) -> bool {
        const CANCELLATION_POLL: Duration = Duration::from_millis(25);

        let mut outstanding = self.outstanding.lock().expect("file completion poisoned");
        while *outstanding != 0 {
            if cancellation.is_cancelled() {
                return false;
            }
            let (next, _) = self
                .changed
                .wait_timeout(outstanding, CANCELLATION_POLL)
                .expect("file completion poisoned");
            outstanding = next;
        }
        !cancellation.is_cancelled()
    }
}

struct FileCompletionTicket {
    completion: Arc<FileCompletion>,
}

impl Drop for FileCompletionTicket {
    fn drop(&mut self) {
        let mut outstanding = self
            .completion
            .outstanding
            .lock()
            .expect("file completion poisoned");
        *outstanding = outstanding
            .checked_sub(1)
            .expect("every file completion ticket is released once");
        self.completion.changed.notify_all();
    }
}

type TimestampResolutionCache =
    Arc<Mutex<HashMap<traverse::MountIdentity, metadata::TimestampResolution>>>;

fn timestamp_resolution_for<Fd: AsFd>(
    fd: Fd,
    mount: traverse::MountIdentity,
    cache: &TimestampResolutionCache,
) -> std::io::Result<metadata::TimestampResolution> {
    if let Some(resolution) = cache
        .lock()
        .expect("timestamp-resolution cache poisoned")
        .get(&mount)
        .copied()
    {
        return Ok(resolution);
    }
    let resolution = metadata::TimestampResolution::for_fd(fd)?;
    cache
        .lock()
        .expect("timestamp-resolution cache poisoned")
        .insert(mount, resolution);
    Ok(resolution)
}

/// Convert the FD-relative traversal observation into the comparison stamp.
///
/// This is intentionally a value conversion, not another `fstat`: the normal
/// metadata no-op path must stay at directory enumeration plus no-follow
/// `statat` calls. A later open is checked against this observation before it
/// authorizes a mutation.
fn metadata_stamp_from_traverse(stamp: traverse::FileStamp) -> metadata::FileStamp {
    let file_type = match stamp.kind {
        traverse::EntryKind::Regular => metadata::FileKind::Regular,
        traverse::EntryKind::Directory => metadata::FileKind::Directory,
        traverse::EntryKind::Symlink => metadata::FileKind::Symlink,
        traverse::EntryKind::Other => metadata::FileKind::Other,
    };
    metadata::FileStamp {
        dev: stamp.device,
        ino: stamp.inode,
        file_type,
        size: stamp.size,
        mode: stamp.mode,
        mtime: metadata::Timestamp::new(
            stamp.mtime.seconds,
            u32::try_from(stamp.mtime.nanoseconds)
                .expect("filesystem timestamp nanoseconds fit in u32"),
        ),
        ctime: metadata::Timestamp::new(
            stamp.ctime.seconds,
            u32::try_from(stamp.ctime.nanoseconds)
                .expect("filesystem timestamp nanoseconds fit in u32"),
        ),
    }
}

/// The default no-op proof deliberately excludes xattrs. V1 propagates them
/// on real mutations, never by opening/listing an otherwise converged file.
fn metadata_noop_proven(
    source: traverse::FileStamp,
    destination: traverse::FileStamp,
    resolution: metadata::TimestampResolution,
) -> bool {
    let source = metadata_stamp_from_traverse(source);
    let destination = metadata_stamp_from_traverse(destination);
    source.size == destination.size
        && source.permission_bits() == destination.permission_bits()
        && metadata::compare_timestamps(source.mtime, destination.mtime, resolution)
            == TimestampComparison::Equal
}

impl FileTask {
    fn ensure_not_cancelled(&self) -> Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return Err(task_conflict(
                "copy",
                &self.display_path,
                "copy worker was cancelled before publication",
            ));
        }
        Ok(())
    }

    fn run(self) -> Result<()> {
        let source_stamp = copy::stamp_fd(&self.source).map_err(|error| {
            task_conflict(
                "stat source file before worker",
                &self.display_path,
                error.to_string(),
            )
        })?;
        #[cfg(test)]
        if let Some(error) = fail_selected_file_worker_for_test(source_stamp) {
            return Err(error);
        }
        if !self.expected_source.source_is_stable(source_stamp) {
            return Err(task_conflict(
                "copy",
                &self.display_path,
                "source changed after Phase A",
            ));
        }
        if self.destination_exists {
            let destination = traverse::open_child_regular_file(
                self.destination_parent.as_ref(),
                &self.name,
                self.options.mount_policy,
            )
            .map_err(|error| task_io("open destination file", &self.display_path, error))?;
            let destination_identity = copy::stamp_fd(&destination).map_err(|error| {
                task_conflict(
                    "stat destination file",
                    &self.display_path,
                    error.to_string(),
                )
            })?;
            let copy::DestinationExpectation::Present(expected_destination) =
                self.expected_destination
            else {
                return Err(task_conflict(
                    "compare regular file",
                    &self.display_path,
                    "destination appeared after Phase A",
                ));
            };
            if !expected_destination.destination_is_unchanged(destination_identity) {
                return Err(task_conflict(
                    "compare regular file",
                    &self.display_path,
                    "destination changed after Phase A",
                ));
            }
            let destination_stamp = metadata::stamp_fd(&destination)
                .map_err(|error| task_io("stat destination file", &self.display_path, error))?;
            let source_stamp = metadata::stamp_fd(&self.source)
                .map_err(|error| task_io("stat source file", &self.display_path, error))?;
            let resolution = timestamp_resolution_for(
                &destination,
                destination.stamp().mount,
                &self.timestamp_resolutions,
            )
            .map_err(|error| {
                task_io(
                    "read destination timestamp resolution",
                    &self.display_path,
                    error,
                )
            })?;
            let comparison_reads_hashes = source_stamp.size == destination_stamp.size
                && (self.options.check == compare::CheckMode::Hash
                    || metadata::compare_timestamps(
                        source_stamp.mtime,
                        destination_stamp.mtime,
                        resolution,
                    ) == TimestampComparison::Ambiguous);
            let comparison = FILE_HASH_BUFFER
                .with(|scratch| {
                    compare::compare_regular_files(
                        &self.source,
                        &destination,
                        source_stamp,
                        destination_stamp,
                        self.options.check,
                        resolution,
                        &mut scratch.borrow_mut(),
                    )
                })
                .map_err(|error| {
                    task_conflict(
                        "compare regular file",
                        &self.display_path,
                        error.to_string(),
                    )
                })?;
            if comparison_reads_hashes {
                self.progress
                    .add_hashed_bytes(source_stamp.size.saturating_add(destination_stamp.size));
            }
            self.progress.record_compared_file();
            if comparison == compare::RegularFileComparison::Equal {
                let source_current = copy::stamp_fd(&self.source).map_err(|error| {
                    task_conflict(
                        "revalidate source file after comparison",
                        &self.display_path,
                        error.to_string(),
                    )
                })?;
                if !self.expected_source.source_is_stable(source_current) {
                    return Err(task_conflict(
                        "revalidate source file after comparison",
                        &self.display_path,
                        "source changed during comparison",
                    ));
                }
                let destination_current = copy::stamp_fd(&destination).map_err(|error| {
                    task_conflict(
                        "revalidate destination file after comparison",
                        &self.display_path,
                        error.to_string(),
                    )
                })?;
                if !expected_destination.destination_is_unchanged(destination_current) {
                    return Err(task_conflict(
                        "revalidate destination file after comparison",
                        &self.display_path,
                        "destination changed during comparison",
                    ));
                }
                copy::check_destination(
                    self.destination_parent.as_ref(),
                    &self.name,
                    self.expected_destination,
                )
                .map_err(|error| {
                    task_conflict(
                        "revalidate destination name before metadata update",
                        &self.display_path,
                        error.to_string(),
                    )
                })?;
                self.ensure_not_cancelled()?;
                converge_regular_metadata(
                    self.options,
                    &self.source,
                    &destination,
                    RegularMetadata {
                        source_stamp,
                        destination_stamp,
                        resolution,
                        name: &self.display_path,
                    },
                )?;
                self.progress.record_completed_file(source_stamp.size);
                self.progress.record_skipped_file();
                return Ok(());
            }
        }

        if self.options.dry_run {
            println!(
                "{}\t{}",
                if self.destination_exists {
                    "update"
                } else {
                    "copy"
                },
                display_component(&self.display_path)
            );
            let source_size = metadata::stamp_fd(&self.source)
                .map_err(|error| {
                    task_io("stat source file after dry-run", &self.display_path, error)
                })?
                .size;
            self.progress.record_completed_file(source_size);
            return Ok(());
        }
        self.ensure_not_cancelled()?;
        let publication = copy::publish_regular_file_checked(
            &self.source,
            self.destination_parent.as_ref(),
            &self.name,
            Some(self.expected_source),
            self.expected_destination,
            copy::PublishOptions {
                durable: self.options.durable,
                clone_capabilities: Some(self.clone_capabilities.clone()),
            },
        )
        .map_err(|error| task_conflict("copy", &self.display_path, error.to_string()))?;
        self.progress
            .record_completed_file(publication.bytes_copied);
        match publication.method {
            crate::platform::CopyMethod::Clone | crate::platform::CopyMethod::Reflink => {
                self.progress.add_cloned_bytes(publication.bytes_copied);
            }
            crate::platform::CopyMethod::CopyFileRange | crate::platform::CopyMethod::Buffered => {
                self.progress.add_streamed_bytes(publication.bytes_copied);
            }
        }
        if self.options.verbose {
            println!(
                "{}\t{}",
                if self.destination_exists {
                    "update"
                } else {
                    "copy"
                },
                display_component(&self.display_path)
            );
        }
        Ok(())
    }
}

fn task_io(operation: &str, name: &[u8], error: impl Into<std::io::Error>) -> FsError {
    FsError::io(operation, &os_name(name), error)
}

fn task_conflict(operation: &str, name: &[u8], reason: impl Into<String>) -> FsError {
    FsError::conflict(operation, &os_name(name), reason)
}

/// The metadata observation that authorizes a non-data regular-file update.
/// Grouping it keeps the mutation boundary explicit: no metadata is applied
/// from a fresh pathname lookup after comparison has completed.
#[derive(Clone, Copy)]
struct RegularMetadata<'a> {
    source_stamp: metadata::FileStamp,
    destination_stamp: metadata::FileStamp,
    resolution: metadata::TimestampResolution,
    name: &'a [u8],
}

/// Apply regular-file metadata only when content comparison already proved
/// equality. This deliberately does not inspect xattrs on a clean no-op.
fn converge_regular_metadata<S: AsFd, D: AsFd>(
    options: RunOptions,
    source: S,
    destination: D,
    metadata: RegularMetadata<'_>,
) -> Result<()> {
    let timestamp = metadata::compare_timestamps(
        metadata.source_stamp.mtime,
        metadata.destination_stamp.mtime,
        metadata.resolution,
    );
    let mode_changed =
        metadata.source_stamp.permission_bits() != metadata.destination_stamp.permission_bits();
    // An unknown-resolution equal-content file is deliberately not touched
    // just to chase an unrepresentable mtime forever.
    let mtime_changed = timestamp == TimestampComparison::Different;
    if !mode_changed && !mtime_changed {
        return Ok(());
    }
    if options.dry_run {
        println!("update\t{}", display_component(metadata.name));
        return Ok(());
    }
    if mode_changed {
        crate::metadata::set_mode_fd(&destination, metadata.source_stamp.mode)
            .map_err(|error| task_io("set file mode", metadata.name, error))?;
    }
    if mtime_changed {
        crate::metadata::set_mtime_fd(&destination, metadata.source_stamp.mtime)
            .map_err(|error| task_io("set file mtime", metadata.name, error))?;
    }
    metadata::propagate_xattrs(&source, &destination)
        .map_err(|error| task_io("propagate file xattrs", metadata.name, error))?;
    if options.durable {
        crate::platform::sync_file_for_durable_publish(&destination)
            .map_err(|error| task_io("sync file metadata", metadata.name, error))?;
    }
    if options.verbose {
        println!("update\t{}", display_component(metadata.name));
    }
    Ok(())
}

/// The bounded regular-file worker set.  Its drop implementation cancels and
/// joins on an early directory-walk error, so queued mutations cannot outlive
/// the operation that authorized them.
struct FileWorkers {
    sender: WorkSender<FileTask>,
    cancellation: Cancellation,
    completion: Arc<FileCompletion>,
    group: Option<WorkerGroup>,
}

impl FileWorkers {
    fn new(jobs: usize) -> Self {
        let cancellation = Cancellation::new();
        let queue =
            WorkQueue::with_cancellation(jobs.saturating_mul(32).max(1), cancellation.clone());
        let sender = queue.sender();
        let receiver = queue.receiver();
        let group = WorkerGroup::spawn(receiver, jobs, cancellation.clone(), FileTask::run);
        Self {
            sender,
            cancellation,
            completion: Arc::new(FileCompletion::default()),
            group: Some(group),
        }
    }

    fn enqueue(&self, mut task: FileTask) -> Result<()> {
        task.completion = Some(self.completion.reserve());
        task.cancellation = Some(self.cancellation.clone());
        match self.sender.send(task) {
            Ok(()) => Ok(()),
            Err(SendError::Closed(_)) => Err(FsError::conflict(
                "schedule copy worker",
                OsStr::new("."),
                "worker queue closed before all file work was scheduled",
            )),
            Err(SendError::Cancelled(_)) => Err(FsError::conflict(
                "schedule copy worker",
                OsStr::new("."),
                "a file worker failed; remaining work was cancelled",
            )),
        }
    }

    /// Wait for the files already scheduled by the single discovery thread.
    /// No new file work is submitted while this method runs, so zero means the
    /// current directory subtree is safe to finalize. A failed worker returns
    /// immediately instead of waiting for cancelled queue entries to drain.
    fn wait_until_idle(&self) -> Result<()> {
        if self
            .completion
            .wait_until_drained_or_cancelled(&self.cancellation)
        {
            return Ok(());
        }
        Err(self.cancellation_error())
    }

    fn ensure_not_cancelled(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            Err(self.cancellation_error())
        } else {
            Ok(())
        }
    }

    fn cancellation_error(&self) -> FsError {
        let reason = self
            .cancellation
            .reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "copy worker was cancelled".to_owned());
        FsError::conflict("copy worker", OsStr::new("."), reason)
    }

    fn finish(mut self) -> Result<()> {
        self.sender.close();
        let group = self
            .group
            .take()
            .expect("worker group is present until finish");
        match group.join() {
            Ok(()) => Ok(()),
            Err(WorkerJoinError::Task(error)) => Err(FsError::conflict(
                "copy worker",
                OsStr::new("."),
                error.to_string(),
            )),
            Err(WorkerJoinError::Cancelled) => Err(FsError::conflict(
                "copy worker",
                OsStr::new("."),
                "copy worker was cancelled",
            )),
            Err(WorkerJoinError::Panicked) => Err(FsError::conflict(
                "copy worker",
                OsStr::new("."),
                "copy worker panicked",
            )),
        }
    }
}

impl Drop for FileWorkers {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(group) = self.group.take() {
            let _ = group.join();
        }
    }
}

struct Engine {
    options: RunOptions,
    hash_buffer: Vec<u8>,
    temp_names: delete::TempNameGenerator,
    progress: Arc<Progress>,
    timestamp_resolutions: TimestampResolutionCache,
    clone_capabilities: Arc<copy::CloneCapabilityCache>,
    workers: Option<FileWorkers>,
}

impl Engine {
    fn new(options: RunOptions, progress: Arc<Progress>) -> Self {
        Self {
            options,
            hash_buffer: vec![0; HASH_BUFFER_SIZE],
            temp_names: delete::TempNameGenerator::new(std::process::id()),
            progress,
            timestamp_resolutions: Arc::new(Mutex::new(HashMap::new())),
            clone_capabilities: Arc::new(copy::CloneCapabilityCache::default()),
            workers: None,
        }
    }

    fn start_file_workers(&mut self) {
        debug_assert!(self.workers.is_none());
        self.workers = Some(FileWorkers::new(self.options.jobs));
    }

    fn finish_file_workers(&mut self) -> Result<()> {
        if let Some(workers) = self.workers.take() {
            workers.finish()?;
        }
        Ok(())
    }

    fn emit(&self, verb: &str, name: &[u8]) {
        if self.options.verbose || self.options.dry_run {
            println!("{verb}\t{}", display_component(name));
        }
    }

    fn emit_child(&self, verb: &str, relative_parent: &[u8], name: &[u8]) {
        if self.options.verbose || self.options.dry_run {
            self.emit(verb, &relative_child_path(relative_parent, name));
        }
    }

    fn publish_options(&self) -> copy::PublishOptions {
        copy::PublishOptions {
            durable: self.options.durable,
            clone_capabilities: Some(self.clone_capabilities.clone()),
        }
    }

    fn io(&self, operation: &str, name: &[u8], error: impl Into<std::io::Error>) -> FsError {
        let name = os_name(name);
        FsError::io(operation, &name, error)
    }

    fn conflict(&self, operation: &str, name: &[u8], reason: impl Into<String>) -> FsError {
        let name = os_name(name);
        FsError::conflict(operation, &name, reason)
    }

    fn phase_a_directory(
        &mut self,
        mut source: traverse::DirectoryFd,
        mut destination: Option<Arc<traverse::DirectoryFd>>,
        relative_path: Vec<u8>,
    ) -> Result<(traverse::DirectoryFd, Option<Arc<traverse::DirectoryFd>>)> {
        // A source directory is the membership proof for this portion of the
        // walk.  Check it on both sides of Phase A so an entry created,
        // removed, or renamed while this directory was being discovered does
        // not let `sync` advance to destructive pruning on an incomplete
        // source view.  This is deliberately an FD re-stat: directory names
        // may be replaced concurrently, but the held descriptor remains the
        // object whose membership we inspected.
        let source_before = traverse::stamp_fd(&source)
            .map_err(|error| self.io("stat source directory before phase A", b".", error))?;
        #[cfg(test)]
        mutate_source_during_phase_a_for_test(&source)
            .map_err(|error| self.io("mutate source during Phase A test", b".", error))?;
        let mut entries = traverse::DirectoryEntries::open(&source)
            .map_err(|error| self.io("enumerate source", b".", error))?;
        // POSIX does not permit a closed directory stream to be resumed on a
        // fresh `DIR *`.  Stream immediate directory-child records to an
        // anonymous spool instead. Regular files and links still flow to
        // their normal mutation path as soon as they are discovered.
        let mut children: Option<DirectoryWorkStorage> = None;
        while let Some(entry) = entries
            .next_entry()
            .map_err(|error| self.io("enumerate source", b".", error))?
        {
            if let Some(workers) = self.workers.as_ref() {
                workers.ensure_not_cancelled()?;
            }
            self.progress.record_scanned_entry();
            let source_stamp =
                traverse::stat_child(&source, &entry.name, self.options.mount_policy).map_err(
                    |error| {
                        if error.kind() == ErrorKind::NotFound {
                            self.conflict(
                                "inspect source",
                                &entry.name,
                                "source entry disappeared after enumeration",
                            )
                        } else {
                            self.io("stat source", &entry.name, error)
                        }
                    },
                )?;
            let destination_stamp = match destination.as_ref() {
                Some(directory) => {
                    match traverse::stat_child(
                        directory.as_ref(),
                        &entry.name,
                        self.options.mount_policy,
                    ) {
                        Ok(stamp) => Some(stamp),
                        Err(error) if error.kind() == ErrorKind::NotFound => None,
                        Err(error) => return Err(self.io("stat destination", &entry.name, error)),
                    }
                }
                None => None,
            };

            if let Some(destination_stamp) = destination_stamp
                && source_stamp.kind != destination_stamp.kind
            {
                return Err(self.conflict(
                    "converge",
                    &entry.name,
                    "source and destination entry types differ",
                ));
            }

            match source_stamp.kind {
                traverse::EntryKind::Regular => self.converge_regular(
                    &source,
                    destination.as_ref(),
                    &entry.name,
                    &relative_path,
                    source_stamp,
                    destination_stamp,
                )?,
                traverse::EntryKind::Symlink => self.converge_symlink(
                    &source,
                    destination.as_ref(),
                    &entry.name,
                    &relative_path,
                    source_stamp,
                    destination_stamp,
                )?,
                traverse::EntryKind::Directory => {
                    let destination = match (destination.as_ref(), destination_stamp) {
                        (Some(_), Some(stamp)) => Some(stamp),
                        (Some(_), None) if self.options.dry_run => {
                            self.emit_child("mkdir", &relative_path, &entry.name);
                            None
                        }
                        (Some(parent), None) => {
                            let source_child = traverse::open_planned_child_directory(
                                &source,
                                &entry.name,
                                source_stamp,
                                self.options.mount_policy,
                            )
                            .map_err(|error| {
                                self.io("open source directory", &entry.name, error)
                            })?;
                            let destination_child = self
                                .create_directory(
                                    parent.as_ref(),
                                    &entry.name,
                                    &relative_child_path(&relative_path, &entry.name),
                                )?
                                .expect("non-dry-run directory creation returns an FD");
                            // Directory xattrs are creation metadata. They
                            // must be copied before this descriptor is closed.
                            metadata::propagate_xattrs(&source_child, &destination_child).map_err(
                                |error| {
                                    self.io(
                                        "propagate created directory xattrs",
                                        &entry.name,
                                        error,
                                    )
                                },
                            )?;
                            Some(destination_child.stamp())
                        }
                        (None, _) => {
                            self.emit_child("mkdir", &relative_path, &entry.name);
                            None
                        }
                    };
                    let record = DirectoryWorkRecord {
                        name: entry.name,
                        source: Some(source_stamp),
                        destination,
                    };
                    if children.is_none() {
                        children = Some(DirectoryWorkStorage::new(self.options.dry_run));
                    }
                    let record_name = record.name.clone();
                    children
                        .as_mut()
                        .expect("directory work storage is created above")
                        .push(record)
                        .map_err(|error| self.io("record directory work", &record_name, error))?;
                }
                traverse::EntryKind::Other => {
                    return Err(self.conflict(
                        "inspect source",
                        &entry.name,
                        "unsupported source inode type",
                    ));
                }
            }
        }
        drop(entries);

        if let Some(children) = children {
            let mut children = children
                .into_reader()
                .map_err(|error| self.io("read directory work", b".", error))?;
            while let Some(record) = children
                .next()
                .map_err(|error| self.io("read directory work", b".", error))?
            {
                if let Some(workers) = self.workers.as_ref() {
                    workers.ensure_not_cancelled()?;
                }
                let expected_source = record.source.ok_or_else(|| {
                    self.conflict(
                        "read directory work",
                        &record.name,
                        "source directory record is missing its source identity",
                    )
                })?;
                let source_child = traverse::open_planned_child_directory(
                    &source,
                    &record.name,
                    expected_source,
                    self.options.mount_policy,
                )
                .map_err(|error| self.io("open source directory", &record.name, error))?;
                let destination_child = match (destination.as_ref(), record.destination) {
                    (Some(parent), Some(expected)) => Some(Arc::new(
                        traverse::open_planned_child_directory(
                            parent.as_ref(),
                            &record.name,
                            expected,
                            self.options.mount_policy,
                        )
                        .map_err(|error| {
                            self.io("open destination directory", &record.name, error)
                        })?,
                    )),
                    (Some(_), None) if self.options.dry_run => None,
                    (None, None) => None,
                    _ => {
                        return Err(self.conflict(
                            "open destination directory",
                            &record.name,
                            "destination child state changed after discovery",
                        ));
                    }
                };
                let destination_identity = destination.as_ref().map(|directory| directory.stamp());
                // An absent dry-run child has no descriptor through which to
                // recover its destination parent. Retain only that read-only
                // capability; normal execution recovers both parents from the
                // returned child descriptors.
                let retained_dry_destination =
                    if destination_child.is_none() && self.options.dry_run {
                        destination.take()
                    } else {
                        None
                    };
                drop(source);
                drop(destination);

                let child_relative_path = relative_child_path(&relative_path, &record.name);
                let (source_child, destination_child) = self.phase_a_directory(
                    source_child,
                    destination_child,
                    child_relative_path.clone(),
                )?;
                self.finalize_phase_a_child(
                    &source_child,
                    destination_child.as_deref(),
                    &child_relative_path,
                )?;
                #[cfg(test)]
                if let Some(destination_child) = destination_child.as_deref() {
                    replace_destination_child_after_phase_a_for_test(
                        destination_child,
                        &record.name,
                    )
                    .map_err(|error| {
                        self.io(
                            "replace destination child during Phase A test",
                            &record.name,
                            error,
                        )
                    })?;
                }

                let returned_source = source_child.stamp();
                source = traverse::open_parent_directory(&source_child).map_err(|error| {
                    self.io("open source parent after child", &record.name, error)
                })?;
                let observed_source_parent = traverse::stamp_fd(&source).map_err(|error| {
                    self.io("stat source parent after child", &record.name, error)
                })?;
                if observed_source_parent != source_before {
                    return Err(self.conflict(
                        "copy source directory",
                        &record.name,
                        "source directory changed while its child was processed",
                    ));
                }
                let source_child_now =
                    traverse::stat_child(&source, &record.name, self.options.mount_policy)
                        .map_err(|error| {
                            self.io("revalidate source child after copy", &record.name, error)
                        })?;
                if !source_child_now.same_object(returned_source) {
                    return Err(self.conflict(
                        "revalidate source child after copy",
                        &record.name,
                        "source child was replaced while it was processed",
                    ));
                }
                destination = match (destination_child, destination_identity) {
                    (Some(destination_child), Some(expected_parent)) => {
                        let returned_destination = destination_child.stamp();
                        let parent = traverse::open_parent_directory(destination_child.as_ref())
                            .map_err(|error| {
                                self.io("open destination parent after child", &record.name, error)
                            })?;
                        if !parent.stamp().same_object(expected_parent) {
                            return Err(self.conflict(
                                "open destination parent after child",
                                &record.name,
                                "destination parent changed while its child was processed",
                            ));
                        }
                        let destination_child_now =
                            traverse::stat_child(&parent, &record.name, self.options.mount_policy)
                                .map_err(|error| {
                                    self.io(
                                        "revalidate destination child after copy",
                                        &record.name,
                                        error,
                                    )
                                })?;
                        if !destination_child_now.same_object(returned_destination) {
                            return Err(self.conflict(
                                "revalidate destination child after copy",
                                &record.name,
                                "destination child was replaced while it was processed",
                            ));
                        }
                        Some(Arc::new(parent))
                    }
                    (None, None) => None,
                    (None, Some(_)) if self.options.dry_run => retained_dry_destination,
                    _ => {
                        return Err(self.conflict(
                            "open destination parent after child",
                            &record.name,
                            "destination child state changed while it was processed",
                        ));
                    }
                };
            }
        }
        let source_after = traverse::stamp_fd(&source)
            .map_err(|error| self.io("stat source directory after phase A", b".", error))?;
        if source_before != source_after {
            return Err(self.conflict(
                "copy source directory",
                b".",
                "source directory changed during Phase A",
            ));
        }
        Ok((source, destination))
    }

    fn finalize_phase_a_child(
        &mut self,
        source: &traverse::DirectoryFd,
        destination: Option<&traverse::DirectoryFd>,
        name: &[u8],
    ) -> Result<()> {
        if self.options.operation == Operation::Cp
            && let Some(destination) = destination
        {
            if let Some(workers) = self.workers.as_ref() {
                workers.wait_until_idle()?;
            }
            self.finalize_directory(source, destination, name)?;
        }
        Ok(())
    }

    fn create_directory(
        &self,
        parent: &traverse::DirectoryFd,
        name: &[u8],
        display_path: &[u8],
    ) -> Result<Option<traverse::DirectoryFd>> {
        fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)).map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                self.conflict(
                    "mkdir",
                    display_path,
                    "destination appeared during convergence",
                )
            } else {
                self.io("mkdir", display_path, error)
            }
        })?;
        self.emit("mkdir", display_path);
        let directory = traverse::open_child_directory(parent, name, self.options.mount_policy)
            .map_err(|error| self.io("open created directory", display_path, error))?;
        if self.options.durable {
            crate::platform::sync_directory_for_durable_metadata(&directory)
                .map_err(|error| self.io("sync created directory", display_path, error))?;
            crate::platform::sync_parent_directory(parent).map_err(|error| {
                self.io("sync directory parent after mkdir", display_path, error)
            })?;
        }
        Ok(Some(directory))
    }

    fn converge_regular(
        &mut self,
        source_parent: &traverse::DirectoryFd,
        destination_parent: Option<&Arc<traverse::DirectoryFd>>,
        name: &[u8],
        relative_parent: &[u8],
        source_stamp: traverse::FileStamp,
        destination_stamp: Option<traverse::FileStamp>,
    ) -> Result<()> {
        let Some(destination_parent) = destination_parent else {
            self.emit_child("copy", relative_parent, name);
            return Ok(());
        };

        if let Some(destination_stamp) = destination_stamp
            && self.options.check == compare::CheckMode::Metadata
            // A mounted child is permitted only with `--cross-file-systems`.
            // Its parent FD reports the parent filesystem's timestamp grid,
            // so defer to the descriptor-opening comparison path instead of
            // applying a potentially wrong cached resolution.
            && destination_stamp.mount == destination_parent.stamp().mount
        {
            let resolution = timestamp_resolution_for(
                destination_parent.as_ref(),
                destination_stamp.mount,
                &self.timestamp_resolutions,
            )
            .map_err(|error| self.io("read destination timestamp resolution", name, error))?;
            if metadata_noop_proven(source_stamp, destination_stamp, resolution) {
                self.progress.record_compared_file();
                self.progress.record_skipped_file();
                return Ok(());
            }
        }
        let source =
            traverse::open_child_regular_file(source_parent, name, self.options.mount_policy)
                .map_err(|error| self.io("open source file", name, error))?;
        let expected_source = copy::FileStamp::from_traverse(source_stamp);
        let opened_source = copy::stamp_fd(&source).map_err(|error| {
            self.conflict("stat source file after open", name, error.to_string())
        })?;
        if !expected_source.source_is_stable(opened_source) {
            return Err(self.conflict(
                "copy source file",
                name,
                "source changed between planning and open",
            ));
        }
        let expected_destination = destination_stamp
            .map(copy::FileStamp::from_traverse)
            .map(copy::DestinationExpectation::Present)
            .unwrap_or(copy::DestinationExpectation::Absent);
        let destination_exists = destination_stamp.is_some();
        let task = FileTask {
            source,
            destination_parent: Arc::clone(destination_parent),
            display_path: relative_child_path(relative_parent, name),
            name: name.to_vec(),
            expected_source,
            expected_destination,
            destination_exists,
            options: self.options,
            progress: self.progress.clone(),
            timestamp_resolutions: self.timestamp_resolutions.clone(),
            clone_capabilities: self.clone_capabilities.clone(),
            cancellation: None,
            completion: None,
        };
        // The source walker, rather than the asynchronous comparison worker,
        // owns the progress denominator. A hash comparison can later prove
        // this candidate unchanged, but it is still work whose completion
        // must advance the already-frozen meter.
        self.progress.record_planned_file(source_stamp.size);
        match self.workers.as_ref() {
            Some(workers) => workers.enqueue(task),
            None => task.run(),
        }
    }

    fn converge_regular_metadata<S: AsFd, D: AsFd>(
        &self,
        source: S,
        destination: D,
        source_stamp: metadata::FileStamp,
        destination_stamp: metadata::FileStamp,
        resolution: metadata::TimestampResolution,
        name: &[u8],
    ) -> Result<()> {
        converge_regular_metadata(
            self.options,
            source,
            destination,
            RegularMetadata {
                source_stamp,
                destination_stamp,
                resolution,
                name,
            },
        )
    }

    fn converge_symlink(
        &mut self,
        source_parent: &traverse::DirectoryFd,
        destination_parent: Option<&Arc<traverse::DirectoryFd>>,
        name: &[u8],
        relative_parent: &[u8],
        source_stamp: traverse::FileStamp,
        destination_stamp: Option<traverse::FileStamp>,
    ) -> Result<()> {
        let Some(destination_parent) = destination_parent else {
            self.emit_child("copy", relative_parent, name);
            return Ok(());
        };
        let destination_exists = destination_stamp.is_some();
        if destination_exists {
            let source_target = traverse::read_symlink(source_parent, name)
                .map_err(|error| self.io("read source symlink", name, error))?;
            let destination_target = traverse::read_symlink(destination_parent.as_ref(), name)
                .map_err(|error| self.io("read destination symlink", name, error))?;
            if source_target == destination_target {
                return Ok(());
            }
        }
        if self.options.dry_run {
            self.emit_child(
                if destination_exists { "update" } else { "copy" },
                relative_parent,
                name,
            );
            return Ok(());
        }
        let expected_source = copy::FileStamp::from_traverse(source_stamp);
        let expected_destination = destination_stamp
            .map(copy::FileStamp::from_traverse)
            .map(copy::DestinationExpectation::Present)
            .unwrap_or(copy::DestinationExpectation::Absent);
        copy::publish_symlink_checked(
            source_parent,
            name,
            destination_parent.as_ref(),
            name,
            Some(expected_source),
            expected_destination,
            self.publish_options(),
        )
        .map_err(|error| self.conflict("copy symlink", name, error.to_string()))?;
        self.emit_child(
            if destination_exists { "update" } else { "copy" },
            relative_parent,
            name,
        );
        Ok(())
    }

    fn finalize_directory(
        &self,
        source: &traverse::DirectoryFd,
        destination: &traverse::DirectoryFd,
        name: &[u8],
    ) -> Result<()> {
        let source_stamp = metadata::stamp_fd(source)
            .map_err(|error| self.io("stat source directory", name, error))?;
        let destination_stamp = metadata::stamp_fd(destination)
            .map_err(|error| self.io("stat destination directory", name, error))?;
        let resolution = timestamp_resolution_for(
            destination,
            destination.stamp().mount,
            &self.timestamp_resolutions,
        )
        .map_err(|error| self.io("read destination timestamp resolution", name, error))?;
        let mtime =
            metadata::compare_timestamps(source_stamp.mtime, destination_stamp.mtime, resolution);
        let mode_changed = source_stamp.permission_bits() != destination_stamp.permission_bits();
        let mtime_changed = mtime == TimestampComparison::Different;
        if !mode_changed && !mtime_changed {
            return Ok(());
        }
        if self.options.dry_run {
            self.emit("update", name);
            return Ok(());
        }
        if mode_changed {
            metadata::set_mode_fd(destination, source_stamp.mode)
                .map_err(|error| self.io("set directory mode", name, error))?;
        }
        // Propagating xattrs is part of a real metadata mutation, never a
        // standalone no-op scan.
        metadata::propagate_xattrs(source, destination)
            .map_err(|error| self.io("propagate directory xattrs", name, error))?;
        if mtime_changed {
            metadata::set_mtime_fd(destination, source_stamp.mtime)
                .map_err(|error| self.io("set directory mtime", name, error))?;
        }
        if self.options.durable {
            crate::platform::sync_directory_for_durable_metadata(destination)
                .map_err(|error| self.io("sync directory metadata", name, error))?;
        }
        self.emit("update", name);
        Ok(())
    }

    fn prune_directory(
        &mut self,
        mut source: traverse::DirectoryFd,
        mut destination: traverse::DirectoryFd,
    ) -> Result<(traverse::DirectoryFd, traverse::DirectoryFd)> {
        let source_before = traverse::stamp_fd(&source)
            .map_err(|error| self.io("stat source before prune", b".", error))?;
        #[cfg(test)]
        mutate_source_during_prune_for_test(&source)
            .map_err(|error| self.io("mutate source during prune test", b".", error))?;
        let mut entries = traverse::DirectoryEntries::open(&destination)
            .map_err(|error| self.io("enumerate destination for prune", b".", error))?;
        // Snapshot every entry before deleting or descending. Readdir is not
        // specified to retain a useful order after a directory mutation, and
        // a closed stream cannot be resumed portably. The anonymous spool
        // gives prune the same bounded-memory continuation as Phase A.
        let mut work = DirectoryWorkStorage::new(self.options.dry_run);
        while let Some(entry) = entries
            .next_entry()
            .map_err(|error| self.io("enumerate destination for prune", b".", error))?
        {
            self.progress.record_scanned_entry();
            let destination_stamp =
                traverse::stat_child(&destination, &entry.name, self.options.mount_policy)
                    .map_err(|error| self.io("stat destination for prune", &entry.name, error))?;
            let source_stamp =
                match traverse::stat_child(&source, &entry.name, self.options.mount_policy) {
                    Ok(stamp) => Some(stamp),
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(error) => return Err(self.io("stat source for prune", &entry.name, error)),
                };
            if let Some(source_stamp) = source_stamp
                && source_stamp.kind != destination_stamp.kind
            {
                return Err(self.conflict(
                    "prune",
                    &entry.name,
                    "source and destination types changed during sync",
                ));
            }
            let record = DirectoryWorkRecord {
                name: entry.name,
                source: source_stamp,
                destination: Some(destination_stamp),
            };
            let record_name = record.name.clone();
            work.push(record)
                .map_err(|error| self.io("record prune work", &record_name, error))?;
        }
        drop(entries);

        let mut work = work
            .into_reader()
            .map_err(|error| self.io("read prune work", b".", error))?;
        while let Some(record) = work
            .next()
            .map_err(|error| self.io("read prune work", b".", error))?
        {
            let destination_stamp = record.destination.ok_or_else(|| {
                self.conflict(
                    "read prune work",
                    &record.name,
                    "destination record is missing its identity",
                )
            })?;
            match record.source {
                None => self.remove_destination_only(
                    &source,
                    &destination,
                    &record.name,
                    destination_stamp,
                )?,
                Some(source_stamp) if source_stamp.kind != traverse::EntryKind::Directory => {}
                Some(source_stamp) => {
                    let source_child = traverse::open_planned_child_directory(
                        &source,
                        &record.name,
                        source_stamp,
                        self.options.mount_policy,
                    )
                    .map_err(|error| {
                        self.io("open source directory during prune", &record.name, error)
                    })?;
                    let destination_child = traverse::open_planned_child_directory(
                        &destination,
                        &record.name,
                        destination_stamp,
                        self.options.mount_policy,
                    )
                    .map_err(|error| {
                        self.io(
                            "open destination directory during prune",
                            &record.name,
                            error,
                        )
                    })?;
                    let expected_destination_parent = destination.stamp();
                    drop(source);
                    drop(destination);
                    let (source_child, destination_child) =
                        self.prune_directory(source_child, destination_child)?;

                    let returned_source = source_child.stamp();
                    source = traverse::open_parent_directory(&source_child).map_err(|error| {
                        self.io("open source parent after prune child", &record.name, error)
                    })?;
                    let observed_source_parent = traverse::stamp_fd(&source).map_err(|error| {
                        self.io("stat source parent after prune child", &record.name, error)
                    })?;
                    if observed_source_parent != source_before {
                        return Err(self.conflict(
                            "prune",
                            &record.name,
                            "source directory changed while its child was pruned",
                        ));
                    }
                    let source_child_now =
                        traverse::stat_child(&source, &record.name, self.options.mount_policy)
                            .map_err(|error| {
                                self.io("revalidate source child after prune", &record.name, error)
                            })?;
                    if !source_child_now.same_object(returned_source) {
                        return Err(self.conflict(
                            "revalidate source child after prune",
                            &record.name,
                            "source child was replaced while it was pruned",
                        ));
                    }

                    let returned_destination = destination_child.stamp();
                    destination =
                        traverse::open_parent_directory(&destination_child).map_err(|error| {
                            self.io(
                                "open destination parent after prune child",
                                &record.name,
                                error,
                            )
                        })?;
                    if !destination.stamp().same_object(expected_destination_parent) {
                        return Err(self.conflict(
                            "prune",
                            &record.name,
                            "destination directory changed while its child was pruned",
                        ));
                    }
                    let destination_child_now =
                        traverse::stat_child(&destination, &record.name, self.options.mount_policy)
                            .map_err(|error| {
                                self.io(
                                    "revalidate destination child after prune",
                                    &record.name,
                                    error,
                                )
                            })?;
                    if !destination_child_now.same_object(returned_destination) {
                        return Err(self.conflict(
                            "revalidate destination child after prune",
                            &record.name,
                            "destination child was replaced while it was pruned",
                        ));
                    }
                }
            }
        }
        let source_after = traverse::stamp_fd(&source)
            .map_err(|error| self.io("stat source after prune", b".", error))?;
        if source_before != source_after {
            return Err(self.conflict("prune", b".", "source directory changed during sync prune"));
        }
        self.finalize_directory(&source, &destination, b".")?;
        Ok((source, destination))
    }

    fn remove_destination_only(
        &mut self,
        source_parent: &traverse::DirectoryFd,
        destination_parent: &traverse::DirectoryFd,
        name: &[u8],
        destination_stamp: traverse::FileStamp,
    ) -> Result<()> {
        // Recheck absence immediately before the destructive operation.
        match traverse::stat_child(source_parent, name, self.options.mount_policy) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(self.conflict(
                    "prune",
                    name,
                    "source counterpart appeared during sync prune",
                ));
            }
            Err(error) => return Err(self.io("revalidate source absence", name, error)),
        }
        if self.options.dry_run {
            self.emit("delete", name);
            return Ok(());
        }
        if destination_stamp.kind == traverse::EntryKind::Directory {
            let mut mount_check =
                |_stamp: &traverse::FileStamp| -> delete::DeleteResult<()> { Ok(()) };
            delete::prune_directory_with_policy(
                destination_parent,
                name,
                destination_stamp,
                &mut self.temp_names,
                &mut mount_check,
                self.options.mount_policy,
            )
            .map_err(|error| {
                self.conflict("delete destination-only directory", name, error.to_string())
            })?;
        } else {
            delete::unlink_checked_with_policy(
                destination_parent,
                name,
                destination_stamp,
                self.options.mount_policy,
            )
            .map_err(|error| {
                self.conflict("delete destination-only entry", name, error.to_string())
            })?;
        }
        if self.options.durable {
            crate::platform::sync_parent_directory(destination_parent)
                .map_err(|error| self.io("sync directory parent after delete", name, error))?;
        }
        self.emit("delete", name);
        self.progress.record_deleted_entry();
        Ok(())
    }
}

/// Execute the selected convergence operation.
pub(crate) fn execute(command: CommandLine) -> Result<()> {
    let options = RunOptions::from(&command);
    let source = path::establish_source(&command.source)?;
    let destination = path::establish_destination(&command.destination)?;
    match path::check_overlap(&source, &destination, "establish roots")? {
        Overlap::SameObject => return Ok(()),
        Overlap::Nested => {
            return Err(FsError::conflict(
                "establish roots",
                &command.destination,
                "source and destination directory trees overlap",
            ));
        }
        Overlap::Disjoint => {}
    }

    let progress = Arc::new(Progress::new());
    let mut renderer = ProgressRenderer::new(progress.clone(), command.options.no_progress);
    renderer.start();
    let mut engine = Engine::new(options, progress.clone());
    let result = match source.metadata().map(|metadata| metadata.kind) {
        Some(RootEntryKind::Directory) => {
            execute_directory_root(&mut engine, &source, &destination)
        }
        Some(RootEntryKind::RegularFile) => {
            execute_regular_root(&mut engine, &source, &destination)
        }
        Some(RootEntryKind::Symlink) => execute_symlink_root(&mut engine, &source, &destination),
        Some(RootEntryKind::Other) => Err(FsError::conflict(
            "inspect source",
            &command.source,
            "unsupported source inode type",
        )),
        None => Err(FsError::invalid_path(
            "establish source",
            &command.source,
            "source root is absent",
        )),
    };
    progress.set_finished(true);
    renderer.finish(result.is_ok());
    result
}

/// Revalidate the root observation made by path establishment and retain the
/// complete no-follow stamp for the publication boundary.  Root metadata is
/// intentionally only an identity summary, so this captures the richer stamp
/// once at the start of root execution and every later publication must still
/// match it.
fn root_expectation(
    engine: &Engine,
    root: &Root,
    operation: &str,
) -> Result<copy::DestinationExpectation> {
    // Child-operation helpers intentionally reject `.` and `..`, while root
    // establishment permits them as meaningful final components (`/` becomes
    // `.`). Re-open those directory roots through the path layer and stamp
    // the held descriptor instead of weakening the child-name contract.
    let observed = if matches!(root.leaf(), value if value == OsStr::new(".") || value == OsStr::new(".."))
    {
        let directory = root.open_directory(operation)?;
        Some(copy::stamp_fd(&directory).map_err(|error| {
            engine.conflict(operation, os_bytes(root.leaf()), error.to_string())
        })?)
    } else {
        copy::stamp_at(root.parent_fd(), os_bytes(root.leaf()))
            .map_err(|error| engine.conflict(operation, os_bytes(root.leaf()), error.to_string()))?
    };
    match (root.metadata(), observed) {
        (None, None) => Ok(copy::DestinationExpectation::Absent),
        (Some(initial), Some(current)) if root_identity_matches(initial, current) => {
            Ok(copy::DestinationExpectation::Present(current))
        }
        (None, Some(_)) => Err(engine.conflict(
            operation,
            os_bytes(root.leaf()),
            "root appeared after establishment",
        )),
        (Some(_), None) => Err(engine.conflict(
            operation,
            os_bytes(root.leaf()),
            "root disappeared after establishment",
        )),
        (Some(_), Some(_)) => Err(engine.conflict(
            operation,
            os_bytes(root.leaf()),
            "root identity changed after establishment",
        )),
    }
}

fn root_identity_matches(initial: path::EntryMetadata, current: copy::FileStamp) -> bool {
    let kind_matches = match initial.kind {
        RootEntryKind::RegularFile => current.file_type == rustix::fs::FileType::RegularFile,
        RootEntryKind::Directory => current.file_type == rustix::fs::FileType::Directory,
        RootEntryKind::Symlink => current.file_type == rustix::fs::FileType::Symlink,
        RootEntryKind::Other => false,
    };
    initial.dev == current.dev && initial.ino == current.ino && kind_matches
}

fn execute_directory_root(engine: &mut Engine, source: &Root, destination: &Root) -> Result<()> {
    let expected_source = match root_expectation(engine, source, "revalidate source root")? {
        copy::DestinationExpectation::Present(stamp) => stamp,
        copy::DestinationExpectation::Absent => {
            return Err(engine.conflict(
                "revalidate source root",
                os_bytes(source.leaf()),
                "source root is absent",
            ));
        }
    };
    let expected_destination =
        root_expectation(engine, destination, "revalidate destination root")?;
    let source = traverse::DirectoryFd::from_owned(source.open_directory("open source root")?)
        .map_err(|error| engine.io("open source root", b".", error))?;
    let opened_source = copy::stamp_fd(&source)
        .map_err(|error| engine.conflict("stat source root after open", b".", error.to_string()))?;
    if !expected_source.source_is_stable(opened_source) {
        return Err(engine.conflict(
            "open source root",
            b".",
            "source root changed between establishment and open",
        ));
    }
    let destination_root_parent = destination.parent_fd();
    let destination_root_leaf = os_bytes(destination.leaf()).to_vec();
    let (destination, destination_created) = match destination
        .metadata()
        .map(|metadata| metadata.kind)
    {
        Some(RootEntryKind::Directory) => {
            let directory = traverse::DirectoryFd::from_owned(
                destination.open_directory("open destination root")?,
            )
            .map_err(|error| engine.io("open destination root", b".", error))?;
            let opened_destination = copy::stamp_fd(&directory).map_err(|error| {
                engine.conflict("stat destination root after open", b".", error.to_string())
            })?;
            let copy::DestinationExpectation::Present(expected_destination) = expected_destination
            else {
                return Err(engine.conflict(
                    "open destination root",
                    b".",
                    "destination root disappeared after establishment",
                ));
            };
            if !expected_destination.destination_is_unchanged(opened_destination) {
                return Err(engine.conflict(
                    "open destination root",
                    b".",
                    "destination root changed between establishment and open",
                ));
            }
            (Some(Arc::new(directory)), false)
        }
        Some(_) => {
            return Err(FsError::conflict(
                "converge root",
                &destination.input,
                "source and destination root types differ",
            ));
        }
        None if engine.options.dry_run => {
            engine.emit("mkdir", os_bytes(destination.leaf()));
            (None, false)
        }
        None => {
            if !matches!(expected_destination, copy::DestinationExpectation::Absent) {
                return Err(engine.conflict(
                    "mkdir destination root",
                    os_bytes(destination.leaf()),
                    "destination root appeared after establishment",
                ));
            }
            fs::mkdirat(
                destination.parent_fd(),
                destination.leaf(),
                Mode::from_raw_mode(0o700),
            )
            .map_err(|error| {
                engine.io(
                    "mkdir destination root",
                    os_bytes(destination.leaf()),
                    error,
                )
            })?;
            engine.emit("mkdir", os_bytes(destination.leaf()));
            (
                Some(Arc::new(
                    open_directory_from_root(destination, engine.options.mount_policy)
                        .map_err(|error| engine.io("open created destination root", b".", error))?,
                )),
                true,
            )
        }
    };
    if destination_created && let Some(destination) = destination.as_ref() {
        metadata::propagate_xattrs(&source, destination.as_ref())
            .map_err(|error| engine.io("propagate created root directory xattrs", b".", error))?;
        if engine.options.durable {
            crate::platform::sync_directory_for_durable_metadata(destination.as_ref())
                .map_err(|error| engine.io("sync created root directory", b".", error))?;
            crate::platform::sync_parent_directory(destination_root_parent).map_err(|error| {
                engine.io(
                    "sync root parent after mkdir",
                    &destination_root_leaf,
                    error,
                )
            })?;
        }
    }
    // Keep a root-level Phase A stamp as well as the per-directory stamps in
    // `phase_a_directory`.  The second check below covers the narrow interval
    // after discovery and worker completion but before Phase B begins.
    let source_before_phase_a = traverse::stamp_fd(&source)
        .map_err(|error| engine.io("stat source root before Phase A", b".", error))?;

    engine.start_file_workers();
    let (source, destination) = engine.phase_a_directory(source, destination, Vec::new())?;
    // Phase A has now discovered every candidate file. Workers may still be
    // comparing or publishing them, but they never extend this denominator,
    // so the renderer can become determinate without a backwards percentage.
    engine.progress.set_discovery_done(true);
    engine.finish_file_workers()?;
    let source_after_phase_a = traverse::stamp_fd(&source)
        .map_err(|error| engine.io("revalidate source root after Phase A", b".", error))?;
    if source_before_phase_a != source_after_phase_a {
        return Err(engine.conflict(
            "copy source root",
            b".",
            "source root changed during Phase A",
        ));
    }
    if let Some(destination) = destination {
        match engine.options.operation {
            Operation::Cp => {
                engine.finalize_directory(&source, destination.as_ref(), b".")?;
            }
            Operation::Sync => {
                let source_before_prune = traverse::stamp_fd(&source).map_err(|error| {
                    engine.io("revalidate source root before prune", b".", error)
                })?;
                if source_before_phase_a != source_before_prune {
                    return Err(engine.conflict(
                        "sync",
                        b".",
                        "source root changed before sync prune",
                    ));
                }
                engine.progress.set_prune_active(true);
                let destination = Arc::try_unwrap(destination).map_err(|_| {
                    engine.conflict(
                        "prune",
                        b".",
                        "destination root is still referenced by copy work",
                    )
                })?;
                let prune_result = engine.prune_directory(source, destination);
                engine.progress.set_prune_active(false);
                let _ = prune_result?;
            }
        }
    }
    Ok(())
}

fn execute_regular_root(engine: &mut Engine, source: &Root, destination: &Root) -> Result<()> {
    let expected_source = match root_expectation(engine, source, "revalidate source root")? {
        copy::DestinationExpectation::Present(stamp) => stamp,
        copy::DestinationExpectation::Absent => {
            return Err(engine.conflict(
                "revalidate source root",
                os_bytes(source.leaf()),
                "source root is absent",
            ));
        }
    };
    let expected_destination =
        root_expectation(engine, destination, "revalidate destination root")?;
    match destination.metadata().map(|metadata| metadata.kind) {
        Some(RootEntryKind::RegularFile) | None => {}
        Some(_) => {
            return Err(FsError::conflict(
                "converge root",
                &destination.input,
                "source and destination root types differ",
            ));
        }
    }
    let source_fd = fs::openat(
        source.parent_fd(),
        source.leaf(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| engine.io("open source root file", os_bytes(source.leaf()), error))?;
    let opened_source = copy::stamp_fd(&source_fd).map_err(|error| {
        engine.conflict(
            "stat source root file after open",
            os_bytes(source.leaf()),
            error.to_string(),
        )
    })?;
    if !expected_source.source_is_stable(opened_source) {
        return Err(engine.conflict(
            "copy root file",
            os_bytes(source.leaf()),
            "source changed between establishment and open",
        ));
    }
    let exists = matches!(
        expected_destination,
        copy::DestinationExpectation::Present(_)
    );
    if exists {
        let destination_fd = fs::openat(
            destination.parent_fd(),
            destination.leaf(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            engine.io(
                "open destination root file",
                os_bytes(destination.leaf()),
                error,
            )
        })?;
        let opened_destination = copy::stamp_fd(&destination_fd).map_err(|error| {
            engine.conflict(
                "stat destination root file after open",
                os_bytes(destination.leaf()),
                error.to_string(),
            )
        })?;
        let copy::DestinationExpectation::Present(expected_destination_stamp) =
            expected_destination
        else {
            unreachable!("root destination existence was established above");
        };
        if !expected_destination_stamp.destination_is_unchanged(opened_destination) {
            return Err(engine.conflict(
                "compare root file",
                os_bytes(destination.leaf()),
                "destination changed after establishment",
            ));
        }
        let source_stamp = metadata::stamp_fd(&source_fd)
            .map_err(|error| engine.io("stat source root file", os_bytes(source.leaf()), error))?;
        let destination_stamp = metadata::stamp_fd(&destination_fd).map_err(|error| {
            engine.io(
                "stat destination root file",
                os_bytes(destination.leaf()),
                error,
            )
        })?;
        let resolution =
            metadata::TimestampResolution::for_fd(&destination_fd).map_err(|error| {
                engine.io(
                    "read destination timestamp resolution",
                    os_bytes(destination.leaf()),
                    error,
                )
            })?;
        let comparison = compare::compare_regular_files(
            &source_fd,
            &destination_fd,
            source_stamp,
            destination_stamp,
            engine.options.check,
            resolution,
            &mut engine.hash_buffer,
        )
        .map_err(|error| {
            engine.conflict(
                "compare root file",
                os_bytes(destination.leaf()),
                error.to_string(),
            )
        })?;
        if comparison == compare::RegularFileComparison::Equal {
            let source_current = copy::stamp_fd(&source_fd).map_err(|error| {
                engine.conflict(
                    "revalidate source root file after comparison",
                    os_bytes(source.leaf()),
                    error.to_string(),
                )
            })?;
            if !expected_source.source_is_stable(source_current) {
                return Err(engine.conflict(
                    "revalidate source root file after comparison",
                    os_bytes(source.leaf()),
                    "source changed during comparison",
                ));
            }
            let destination_current = copy::stamp_fd(&destination_fd).map_err(|error| {
                engine.conflict(
                    "revalidate destination root file after comparison",
                    os_bytes(destination.leaf()),
                    error.to_string(),
                )
            })?;
            if !expected_destination_stamp.destination_is_unchanged(destination_current) {
                return Err(engine.conflict(
                    "revalidate destination root file after comparison",
                    os_bytes(destination.leaf()),
                    "destination changed during comparison",
                ));
            }
            copy::check_destination(
                destination.parent_fd(),
                os_bytes(destination.leaf()),
                expected_destination,
            )
            .map_err(|error| {
                engine.conflict(
                    "revalidate destination root name before metadata update",
                    os_bytes(destination.leaf()),
                    error.to_string(),
                )
            })?;
            return engine.converge_regular_metadata(
                &source_fd,
                &destination_fd,
                source_stamp,
                destination_stamp,
                resolution,
                os_bytes(destination.leaf()),
            );
        }
    }
    if engine.options.dry_run {
        engine.emit(
            if exists { "update" } else { "copy" },
            os_bytes(destination.leaf()),
        );
        return Ok(());
    }
    copy::publish_regular_file_checked(
        &source_fd,
        destination.parent_fd(),
        os_bytes(destination.leaf()),
        Some(expected_source),
        expected_destination,
        engine.publish_options(),
    )
    .map_err(|error| {
        engine.conflict(
            "copy root file",
            os_bytes(destination.leaf()),
            error.to_string(),
        )
    })?;
    engine.emit(
        if exists { "update" } else { "copy" },
        os_bytes(destination.leaf()),
    );
    Ok(())
}

fn execute_symlink_root(engine: &mut Engine, source: &Root, destination: &Root) -> Result<()> {
    let expected_source = match root_expectation(engine, source, "revalidate source root")? {
        copy::DestinationExpectation::Present(stamp) => stamp,
        copy::DestinationExpectation::Absent => {
            return Err(engine.conflict(
                "revalidate source root",
                os_bytes(source.leaf()),
                "source root is absent",
            ));
        }
    };
    let expected_destination =
        root_expectation(engine, destination, "revalidate destination root")?;
    match destination.metadata().map(|metadata| metadata.kind) {
        Some(RootEntryKind::Symlink) | None => {}
        Some(_) => {
            return Err(FsError::conflict(
                "converge root",
                &destination.input,
                "source and destination root types differ",
            ));
        }
    }
    let exists = matches!(
        expected_destination,
        copy::DestinationExpectation::Present(_)
    );
    if exists {
        let source_target = traverse::read_symlink(source.parent_fd(), os_bytes(source.leaf()))
            .map_err(|error| {
                engine.io("read source root symlink", os_bytes(source.leaf()), error)
            })?;
        let destination_target =
            traverse::read_symlink(destination.parent_fd(), os_bytes(destination.leaf())).map_err(
                |error| {
                    engine.io(
                        "read destination root symlink",
                        os_bytes(destination.leaf()),
                        error,
                    )
                },
            )?;
        if source_target == destination_target {
            return Ok(());
        }
    }
    if engine.options.dry_run {
        engine.emit(
            if exists { "update" } else { "copy" },
            os_bytes(destination.leaf()),
        );
        return Ok(());
    }
    copy::publish_symlink_checked(
        source.parent_fd(),
        os_bytes(source.leaf()),
        destination.parent_fd(),
        os_bytes(destination.leaf()),
        Some(expected_source),
        expected_destination,
        engine.publish_options(),
    )
    .map_err(|error| {
        engine.conflict(
            "copy root symlink",
            os_bytes(destination.leaf()),
            error.to_string(),
        )
    })?;
    engine.emit(
        if exists { "update" } else { "copy" },
        os_bytes(destination.leaf()),
    );
    Ok(())
}

fn open_directory_from_root(
    root: &Root,
    policy: traverse::MountPolicy,
) -> std::io::Result<traverse::DirectoryFd> {
    traverse::open_child_directory(root.parent_fd(), os_bytes(root.leaf()), policy)
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or_default().as_bytes()
}

#[cfg(unix)]
fn os_name(value: &[u8]) -> OsString {
    OsString::from_vec(value.to_vec())
}

#[cfg(not(unix))]
fn os_name(value: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(value).into_owned())
}

fn display_component(value: &[u8]) -> String {
    crate::error::escape_os(&os_name(value))
}

/// Build a relative display path lazily for an operation that is already
/// visible or queued. Namespace lookups never receive this multi-component
/// representation.
fn relative_child_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(parent.len() + 1 + name.len());
    path.extend_from_slice(parent);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Options;
    use rustix::fs as rustix_fs;
    use std::fs as std_fs;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".fs-sync-test-{}-{nonce}-{counter}", std::process::id());
        std_fs::create_dir(&name).expect("create fixture");
        std::path::PathBuf::from(name)
    }

    fn command(operation: Operation, source: &str, destination: &str) -> CommandLine {
        CommandLine {
            operation,
            options: Options::default(),
            source: source.into(),
            destination: destination.into(),
        }
    }

    fn command_with_jobs(
        operation: Operation,
        source: &str,
        destination: &str,
        jobs: usize,
    ) -> CommandLine {
        let mut command = command(operation, source, destination);
        command.options.jobs = Some(jobs.try_into().expect("positive worker count"));
        command
    }

    #[cfg(target_os = "linux")]
    const TEST_XATTR: &[u8] = b"user.fs.contract";
    #[cfg(target_os = "macos")]
    const TEST_XATTR: &[u8] = b"com.fs.contract";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const TEST_XATTR: &[u8] = b"user.fs.contract";

    fn set_xattr(path: &std::path::Path, value: &[u8]) {
        let file = std_fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open xattr fixture");
        rustix_fs::fsetxattr(&file, TEST_XATTR, value, rustix_fs::XattrFlags::empty())
            .expect("set xattr");
    }

    fn get_xattr(path: &std::path::Path) -> Vec<u8> {
        let file = std_fs::File::open(path).expect("open xattr fixture");
        let mut buffer = vec![0; 1024];
        let length = rustix_fs::fgetxattr(&file, TEST_XATTR, &mut buffer[..]).expect("get xattr");
        buffer.truncate(length);
        buffer
    }

    #[test]
    fn cp_overlays_and_sync_prunes_without_traversing_links() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(source.join("nested")).unwrap();
        std_fs::write(source.join("file"), b"new").unwrap();
        std_fs::write(source.join("nested/item"), b"nested").unwrap();
        std::os::unix::fs::symlink("nested/item", source.join("current")).unwrap();
        std_fs::create_dir(&destination).unwrap();
        std_fs::write(destination.join("stale"), b"stale").unwrap();

        execute(command(
            Operation::Cp,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .unwrap();
        assert_eq!(std_fs::read(destination.join("file")).unwrap(), b"new");
        assert_eq!(
            std_fs::read(destination.join("nested/item")).unwrap(),
            b"nested"
        );
        assert_eq!(
            std_fs::read_link(destination.join("current")).unwrap(),
            std::path::Path::new("nested/item")
        );
        assert!(destination.join("stale").exists());

        execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .unwrap();
        assert!(!destination.join("stale").exists());
        // A completed operation is idempotent even when it contains files,
        // directories, and symlink objects together.
        execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .unwrap();
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cp_waits_for_workers_before_applying_restrictive_directory_mode() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        for number in 0..96 {
            std_fs::write(source.join(format!("file-{number}")), b"parallel payload").unwrap();
        }
        std_fs::set_permissions(&source, std_fs::Permissions::from_mode(0o500)).unwrap();
        std_fs::create_dir(&destination).unwrap();

        execute(command_with_jobs(
            Operation::Cp,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
            4,
        ))
        .unwrap();

        assert_eq!(
            std_fs::read(destination.join("file-95")).unwrap(),
            b"parallel payload"
        );
        assert_eq!(
            std_fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o500
        );
        std_fs::set_permissions(&source, std_fs::Permissions::from_mode(0o700)).unwrap();
        std_fs::set_permissions(&destination, std_fs::Permissions::from_mode(0o700)).unwrap();
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn xattr_propagates_on_creation_but_xattr_only_drift_is_a_noop() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(&destination).unwrap();
        let source_file = source.join("file");
        let destination_file = destination.join("file");
        std_fs::write(&source_file, b"payload").unwrap();
        set_xattr(&source_file, b"source");

        execute(command(
            Operation::Cp,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .unwrap();
        assert_eq!(get_xattr(&destination_file), b"source");

        set_xattr(&destination_file, b"destination-only-drift");
        execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .unwrap();
        assert_eq!(get_xattr(&destination_file), b"destination-only-drift");
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_directory_mutation_during_prune_aborts_further_convergence() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(&destination).unwrap();
        let source_metadata = std_fs::metadata(&source).unwrap();
        *PRUNE_MUTATION_TARGET.lock().expect("prune mutation target") =
            Some((source_metadata.dev(), source_metadata.ino()));

        let error = execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .expect_err("a source directory changed during prune");
        assert!(
            error
                .to_string()
                .contains("source directory changed during sync prune")
        );
        assert!(source.join(".fs-test-source-prune-mutation").exists());
        *PRUNE_MUTATION_TARGET.lock().expect("prune mutation target") = None;
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_directory_mutation_during_phase_a_prevents_sync_prune() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(&destination).unwrap();
        std_fs::write(
            destination.join("destination-only"),
            b"retain on source race",
        )
        .unwrap();
        let source_metadata = std_fs::metadata(&source).unwrap();
        *PHASE_A_MUTATION_TARGET
            .lock()
            .expect("Phase A mutation target") =
            Some((source_metadata.dev(), source_metadata.ino()));

        let error = execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .expect_err("a source directory changed during Phase A");
        assert!(
            error
                .to_string()
                .contains("source directory changed during Phase A")
        );
        assert!(destination.join("destination-only").exists());
        *PHASE_A_MUTATION_TARGET
            .lock()
            .expect("Phase A mutation target") = None;
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_copy_failure_prevents_sync_prune() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(&destination).unwrap();
        let payload = source.join("payload");
        std_fs::write(&payload, b"copy failure target").unwrap();
        std_fs::write(destination.join("stale"), b"must survive failed Phase A").unwrap();
        let metadata = std_fs::metadata(&payload).unwrap();
        *FILE_WORKER_FAILURE_TARGET
            .lock()
            .expect("file worker failure target") = Some((metadata.dev(), metadata.ino()));

        let error = execute(command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .expect_err("a Phase A worker failure must stop before prune");
        assert!(error.to_string().contains("injected worker I/O failure"));
        assert!(
            destination.join("stale").exists(),
            "sync must not enter destructive Phase B after a copy worker fails"
        );
        *FILE_WORKER_FAILURE_TARGET
            .lock()
            .expect("file worker failure target") = None;
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn destination_child_replacement_after_recursion_is_a_conflict() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(source.join("child")).unwrap();
        std_fs::write(source.join("child/payload"), b"source payload").unwrap();
        std_fs::create_dir(&destination).unwrap();
        std_fs::create_dir(destination.join("child")).unwrap();
        let metadata = std_fs::metadata(destination.join("child")).unwrap();
        *DESTINATION_CHILD_REPLACEMENT_TARGET
            .lock()
            .expect("destination child replacement target") =
            Some((metadata.dev(), metadata.ino()));

        let error = execute(command(
            Operation::Cp,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .expect_err("replacing the destination child name must be a conflict");
        assert!(
            error
                .to_string()
                .contains("destination child was replaced while it was processed")
        );
        assert!(destination.join("child").is_dir());
        assert!(
            destination
                .join(".fs-test-destination-child-parked")
                .is_dir()
        );
        *DESTINATION_CHILD_REPLACEMENT_TARGET
            .lock()
            .expect("destination child replacement target") = None;
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_conflict_fails_without_replacing_destination() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::create_dir(&destination).unwrap();
        std_fs::write(source.join("same"), b"file").unwrap();
        std_fs::create_dir(destination.join("same")).unwrap();

        let error = execute(command(
            Operation::Cp,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        ))
        .expect_err("type mismatch must fail");
        assert_eq!(error.exit_code(), 1);
        assert!(destination.join("same").is_dir());
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dry_run_does_not_create_a_destination_root() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std_fs::create_dir(&source).unwrap();
        std_fs::write(source.join("file"), b"data").unwrap();
        let mut command = command(
            Operation::Sync,
            source.to_str().unwrap(),
            destination.to_str().unwrap(),
        );
        command.options.dry_run = true;
        execute(command).unwrap();
        assert!(!destination.exists());
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regular_and_symlink_roots_are_idempotent() {
        let root = fixture();
        let source_file = root.join("source-file");
        let destination_file = root.join("destination-file");
        std_fs::write(&source_file, b"root file").unwrap();
        execute(command(
            Operation::Cp,
            source_file.to_str().unwrap(),
            destination_file.to_str().unwrap(),
        ))
        .unwrap();
        execute(command(
            Operation::Cp,
            source_file.to_str().unwrap(),
            destination_file.to_str().unwrap(),
        ))
        .unwrap();

        let source_link = root.join("source-link");
        let destination_link = root.join("destination-link");
        std::os::unix::fs::symlink("source-file", &source_link).unwrap();
        execute(command(
            Operation::Cp,
            source_link.to_str().unwrap(),
            destination_link.to_str().unwrap(),
        ))
        .unwrap();
        execute(command(
            Operation::Cp,
            source_link.to_str().unwrap(),
            destination_link.to_str().unwrap(),
        ))
        .unwrap();
        assert_eq!(
            std_fs::read_link(destination_link).unwrap(),
            std::path::Path::new("source-file")
        );
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn planned_destination_stamp_rejects_a_replacement_before_worker_copy() {
        let root = fixture();
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        std_fs::create_dir(&source_path).unwrap();
        std_fs::create_dir(&destination_path).unwrap();
        std_fs::write(source_path.join("file"), b"source bytes").unwrap();
        std_fs::write(destination_path.join("file"), b"planned destination").unwrap();

        let source_parent =
            traverse::DirectoryFd::from_owned(std_fs::File::open(&source_path).unwrap().into())
                .unwrap();
        let destination_parent = traverse::DirectoryFd::from_owned(
            std_fs::File::open(&destination_path).unwrap().into(),
        )
        .unwrap();
        let source_stamp =
            traverse::stat_child(&source_parent, b"file", traverse::MountPolicy::StayOnMount)
                .unwrap();
        let destination_stamp = traverse::stat_child(
            &destination_parent,
            b"file",
            traverse::MountPolicy::StayOnMount,
        )
        .unwrap();
        let source = traverse::open_child_regular_file(
            &source_parent,
            b"file",
            traverse::MountPolicy::StayOnMount,
        )
        .unwrap();

        // This models a third party replacing the destination after Phase A
        // but before the queued worker reaches it. The worker must leave the
        // new object untouched rather than re-stat it as a new expectation.
        std_fs::write(destination_path.join("file"), b"third-party update").unwrap();
        let task = FileTask {
            source,
            destination_parent: Arc::new(destination_parent),
            display_path: b"file".to_vec(),
            name: b"file".to_vec(),
            expected_source: copy::FileStamp::from_traverse(source_stamp),
            expected_destination: copy::DestinationExpectation::Present(
                copy::FileStamp::from_traverse(destination_stamp),
            ),
            destination_exists: true,
            options: RunOptions {
                operation: Operation::Cp,
                check: compare::CheckMode::Metadata,
                mount_policy: traverse::MountPolicy::StayOnMount,
                dry_run: false,
                verbose: false,
                durable: false,
                jobs: 1,
            },
            progress: Arc::new(Progress::new()),
            timestamp_resolutions: Arc::new(Mutex::new(HashMap::new())),
            clone_capabilities: Arc::new(copy::CloneCapabilityCache::default()),
            cancellation: None,
            completion: None,
        };
        let error = task
            .run()
            .expect_err("worker must reject a destination changed after Phase A");
        assert!(
            error
                .to_string()
                .contains("destination changed after Phase A")
        );
        assert_eq!(
            std_fs::read(destination_path.join("file")).unwrap(),
            b"third-party update"
        );
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn planned_source_stamp_rejects_a_path_replacement_before_open() {
        let root = fixture();
        let source_path = root.join("source");
        std_fs::create_dir(&source_path).unwrap();
        std_fs::write(source_path.join("file"), b"planned source").unwrap();
        let source_parent =
            traverse::DirectoryFd::from_owned(std_fs::File::open(&source_path).unwrap().into())
                .unwrap();
        let planned =
            traverse::stat_child(&source_parent, b"file", traverse::MountPolicy::StayOnMount)
                .unwrap();

        std_fs::write(source_path.join("replacement"), b"replacement source").unwrap();
        std_fs::rename(source_path.join("replacement"), source_path.join("file")).unwrap();
        let opened = traverse::open_child_regular_file(
            &source_parent,
            b"file",
            traverse::MountPolicy::StayOnMount,
        )
        .unwrap();
        let expected = copy::FileStamp::from_traverse(planned);
        let observed = copy::stamp_fd(&opened).unwrap();
        assert!(
            !expected.source_is_stable(observed),
            "the convergence path must reject this before queuing a worker"
        );
        std_fs::remove_dir_all(root).unwrap();
    }
}
