//! `cp` overlay and two-phase `sync` convergence.
//!
//! This is deliberately the orchestration layer, not a second filesystem
//! abstraction.  Roots have already been established through held directory
//! descriptors by `path`; every operation below passes one component to the
//! traversal, copy, or deletion primitives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex};

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

thread_local! {
    /// Each worker owns one reusable BLAKE3 read buffer. This bounds memory
    /// while avoiding a megabyte allocation per compared file.
    static FILE_HASH_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; HASH_BUFFER_SIZE]);
}

#[cfg(test)]
static PRUNE_MUTATION_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);
#[cfg(test)]
static PHASE_A_MUTATION_TARGET: Mutex<Option<(u64, u64)>> = Mutex::new(None);

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
    destination_parent: traverse::DirectoryFd,
    name: Vec<u8>,
    destination_exists: bool,
    options: RunOptions,
    progress: Arc<Progress>,
    timestamp_resolutions: TimestampResolutionCache,
    clone_capabilities: Arc<copy::CloneCapabilityCache>,
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

impl FileTask {
    fn run(self) -> Result<()> {
        if self.destination_exists {
            let destination = traverse::open_child_regular_file(
                &self.destination_parent,
                &self.name,
                self.options.mount_policy,
            )
            .map_err(|error| task_io("open destination file", &self.name, error))?;
            let source_stamp = metadata::stamp_fd(&self.source)
                .map_err(|error| task_io("stat source file", &self.name, error))?;
            let destination_stamp = metadata::stamp_fd(&destination)
                .map_err(|error| task_io("stat destination file", &self.name, error))?;
            let resolution = timestamp_resolution_for(
                &destination,
                destination.stamp().mount,
                &self.timestamp_resolutions,
            )
            .map_err(|error| task_io("read destination timestamp resolution", &self.name, error))?;
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
                    task_conflict("compare regular file", &self.name, error.to_string())
                })?;
            self.progress.record_compared_file();
            if comparison == compare::RegularFileComparison::Equal {
                converge_regular_metadata(
                    self.options,
                    &self.progress,
                    &self.source,
                    &destination,
                    source_stamp,
                    destination_stamp,
                    resolution,
                    &self.name,
                )?;
                self.progress.record_skipped_file();
                return Ok(());
            }
        }

        let source_size = metadata::stamp_fd(&self.source)
            .map_err(|error| task_io("stat source file before copy", &self.name, error))?
            .size;
        self.progress.record_planned_file(source_size);
        let expected = copy::stamp_at(&self.destination_parent, &self.name)
            .map_err(|error| {
                task_conflict(
                    "stat destination before copy",
                    &self.name,
                    error.to_string(),
                )
            })?
            .map(copy::DestinationExpectation::Present)
            .unwrap_or(copy::DestinationExpectation::Absent);
        let publication = copy::publish_regular_file_with_options(
            &self.source,
            &self.destination_parent,
            &self.name,
            expected,
            copy::PublishOptions {
                durable: self.options.durable,
                clone_capabilities: Some(self.clone_capabilities.clone()),
            },
        )
        .map_err(|error| task_conflict("copy", &self.name, error.to_string()))?;
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
                display_component(&self.name)
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

/// Apply regular-file metadata only when content comparison already proved
/// equality. This deliberately does not inspect xattrs on a clean no-op.
fn converge_regular_metadata<S: AsFd, D: AsFd>(
    options: RunOptions,
    progress: &Progress,
    source: S,
    destination: D,
    source_stamp: metadata::FileStamp,
    destination_stamp: metadata::FileStamp,
    resolution: metadata::TimestampResolution,
    name: &[u8],
) -> Result<()> {
    let timestamp =
        metadata::compare_timestamps(source_stamp.mtime, destination_stamp.mtime, resolution);
    let mode_changed = source_stamp.permission_bits() != destination_stamp.permission_bits();
    // An unknown-resolution equal-content file is deliberately not touched
    // just to chase an unrepresentable mtime forever.
    let mtime_changed = timestamp == TimestampComparison::Different;
    if !mode_changed && !mtime_changed {
        return Ok(());
    }
    if options.dry_run {
        if options.verbose {
            println!("update\t{}", display_component(name));
        }
        return Ok(());
    }
    if mode_changed {
        metadata::set_mode_fd(&destination, source_stamp.mode)
            .map_err(|error| task_io("set file mode", name, error))?;
    }
    if mtime_changed {
        metadata::set_mtime_fd(&destination, source_stamp.mtime)
            .map_err(|error| task_io("set file mtime", name, error))?;
    }
    metadata::propagate_xattrs(&source, &destination)
        .map_err(|error| task_io("propagate file xattrs", name, error))?;
    if options.verbose {
        println!("update\t{}", display_component(name));
    }
    progress.record_completed_file(0);
    Ok(())
}

/// The bounded regular-file worker set.  Its drop implementation cancels and
/// joins on an early directory-walk error, so queued mutations cannot outlive
/// the operation that authorized them.
struct FileWorkers {
    sender: WorkSender<FileTask>,
    cancellation: Cancellation,
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
            group: Some(group),
        }
    }

    fn enqueue(&self, task: FileTask) -> Result<()> {
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

struct DirectoryFinalizer {
    source: traverse::DirectoryFd,
    destination: traverse::DirectoryFd,
    name: Vec<u8>,
}

struct Engine {
    options: RunOptions,
    hash_buffer: Vec<u8>,
    temp_names: delete::TempNameGenerator,
    progress: Arc<Progress>,
    timestamp_resolutions: TimestampResolutionCache,
    clone_capabilities: Arc<copy::CloneCapabilityCache>,
    workers: Option<FileWorkers>,
    deferred_directory_finalizers: Vec<DirectoryFinalizer>,
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
            deferred_directory_finalizers: Vec::new(),
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

    fn finalize_deferred_directories(&mut self) -> Result<()> {
        let finalizers = std::mem::take(&mut self.deferred_directory_finalizers);
        for finalizer in finalizers {
            self.finalize_directory(&finalizer.source, &finalizer.destination, &finalizer.name)?;
        }
        Ok(())
    }

    fn emit(&self, verb: &str, name: &[u8]) {
        if self.options.verbose || self.options.dry_run {
            println!("{verb}\t{}", display_component(name));
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
        source: &traverse::DirectoryFd,
        destination: Option<&traverse::DirectoryFd>,
    ) -> Result<()> {
        // A source directory is the membership proof for this portion of the
        // walk.  Check it on both sides of Phase A so an entry created,
        // removed, or renamed while this directory was being discovered does
        // not let `sync` advance to destructive pruning on an incomplete
        // source view.  This is deliberately an FD re-stat: directory names
        // may be replaced concurrently, but the held descriptor remains the
        // object whose membership we inspected.
        let source_before = traverse::stamp_fd(source)
            .map_err(|error| self.io("stat source directory before phase A", b".", error))?;
        #[cfg(test)]
        mutate_source_during_phase_a_for_test(source)
            .map_err(|error| self.io("mutate source during Phase A test", b".", error))?;
        for entry in
            traverse::enumerate(source).map_err(|error| self.io("enumerate source", b".", error))?
        {
            self.progress.record_scanned_entry();
            let source_stamp = traverse::stat_child(source, &entry.name, self.options.mount_policy)
                .map_err(|error| self.io("stat source", &entry.name, error))?;
            let destination_stamp = match destination {
                Some(directory) => {
                    match traverse::stat_child(directory, &entry.name, self.options.mount_policy) {
                        Ok(stamp) => Some(stamp),
                        Err(error) if error.kind() == ErrorKind::NotFound => None,
                        Err(error) => return Err(self.io("stat destination", &entry.name, error)),
                    }
                }
                None => None,
            };

            if let Some(destination_stamp) = destination_stamp
                && source_stamp.kind != destination_stamp.kind {
                    return Err(self.conflict(
                        "converge",
                        &entry.name,
                        "source and destination entry types differ",
                    ));
                }

            match source_stamp.kind {
                traverse::EntryKind::Regular => self.converge_regular(
                    source,
                    destination,
                    &entry.name,
                    destination_stamp.is_some(),
                )?,
                traverse::EntryKind::Symlink => self.converge_symlink(
                    source,
                    destination,
                    &entry.name,
                    destination_stamp.is_some(),
                )?,
                traverse::EntryKind::Directory => {
                    let source_child = traverse::open_child_directory(
                        source,
                        &entry.name,
                        self.options.mount_policy,
                    )
                    .map_err(|error| self.io("open source directory", &entry.name, error))?;
                    let destination_child = match destination {
                        Some(parent) if destination_stamp.is_some() => Some(
                            traverse::open_child_directory(
                                parent,
                                &entry.name,
                                self.options.mount_policy,
                            )
                            .map_err(|error| {
                                self.io("open destination directory", &entry.name, error)
                            })?,
                        ),
                        Some(parent) => {
                            if self.options.dry_run {
                                self.emit("mkdir", &entry.name);
                                None
                            } else {
                                self.create_directory(parent, &entry.name)?
                            }
                        }
                        None => {
                            self.emit("mkdir", &entry.name);
                            None
                        }
                    };

                    self.phase_a_directory(&source_child, destination_child.as_ref())?;
                    if self.options.operation == Operation::Cp
                        && let Some(destination_child) = destination_child.as_ref() {
                            if self.workers.is_some() {
                                self.deferred_directory_finalizers.push(DirectoryFinalizer {
                                    source: source_child.try_clone().map_err(|error| {
                                        self.io("duplicate source directory", &entry.name, error)
                                    })?,
                                    destination: destination_child.try_clone().map_err(
                                        |error| {
                                            self.io(
                                                "duplicate destination directory",
                                                &entry.name,
                                                error,
                                            )
                                        },
                                    )?,
                                    name: entry.name.clone(),
                                });
                            } else {
                                self.finalize_directory(
                                    &source_child,
                                    destination_child,
                                    &entry.name,
                                )?;
                            }
                        }
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
        let source_after = traverse::stamp_fd(source)
            .map_err(|error| self.io("stat source directory after phase A", b".", error))?;
        if source_before != source_after {
            return Err(self.conflict(
                "copy source directory",
                b".",
                "source directory changed during Phase A",
            ));
        }
        Ok(())
    }

    fn create_directory(
        &self,
        parent: &traverse::DirectoryFd,
        name: &[u8],
    ) -> Result<Option<traverse::DirectoryFd>> {
        fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)).map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                self.conflict("mkdir", name, "destination appeared during convergence")
            } else {
                self.io("mkdir", name, error)
            }
        })?;
        self.emit("mkdir", name);
        traverse::open_child_directory(parent, name, self.options.mount_policy)
            .map(Some)
            .map_err(|error| self.io("open created directory", name, error))
    }

    fn converge_regular(
        &mut self,
        source_parent: &traverse::DirectoryFd,
        destination_parent: Option<&traverse::DirectoryFd>,
        name: &[u8],
        destination_exists: bool,
    ) -> Result<()> {
        let Some(destination_parent) = destination_parent else {
            self.emit("copy", name);
            return Ok(());
        };

        if self.options.dry_run {
            self.emit(if destination_exists { "update" } else { "copy" }, name);
            return Ok(());
        }
        let source =
            traverse::open_child_regular_file(source_parent, name, self.options.mount_policy)
                .map_err(|error| self.io("open source file", name, error))?;
        let destination_parent = destination_parent
            .try_clone()
            .map_err(|error| self.io("duplicate destination directory", name, error))?;
        let task = FileTask {
            source,
            destination_parent,
            name: name.to_vec(),
            destination_exists,
            options: self.options,
            progress: self.progress.clone(),
            timestamp_resolutions: self.timestamp_resolutions.clone(),
            clone_capabilities: self.clone_capabilities.clone(),
        };
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
            &self.progress,
            source,
            destination,
            source_stamp,
            destination_stamp,
            resolution,
            name,
        )
    }

    fn converge_symlink(
        &mut self,
        source_parent: &traverse::DirectoryFd,
        destination_parent: Option<&traverse::DirectoryFd>,
        name: &[u8],
        destination_exists: bool,
    ) -> Result<()> {
        let Some(destination_parent) = destination_parent else {
            self.emit("copy", name);
            return Ok(());
        };
        if destination_exists {
            let source_target = traverse::read_symlink(source_parent, name)
                .map_err(|error| self.io("read source symlink", name, error))?;
            let destination_target = traverse::read_symlink(destination_parent, name)
                .map_err(|error| self.io("read destination symlink", name, error))?;
            if source_target == destination_target {
                return Ok(());
            }
        }
        if self.options.dry_run {
            self.emit(if destination_exists { "update" } else { "copy" }, name);
            return Ok(());
        }
        let expected = copy::stamp_at(destination_parent, name)
            .map_err(|error| {
                self.conflict(
                    "stat destination before symlink copy",
                    name,
                    error.to_string(),
                )
            })?
            .map(copy::DestinationExpectation::Present)
            .unwrap_or(copy::DestinationExpectation::Absent);
        copy::publish_symlink(source_parent, name, destination_parent, name, expected)
            .map_err(|error| self.conflict("copy symlink", name, error.to_string()))?;
        self.emit(if destination_exists { "update" } else { "copy" }, name);
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
        self.emit("update", name);
        Ok(())
    }

    fn prune_directory(
        &mut self,
        source: &traverse::DirectoryFd,
        destination: &traverse::DirectoryFd,
    ) -> Result<()> {
        let source_before = traverse::stamp_fd(source)
            .map_err(|error| self.io("stat source before prune", b".", error))?;
        #[cfg(test)]
        mutate_source_during_prune_for_test(source)
            .map_err(|error| self.io("mutate source during prune test", b".", error))?;
        for entry in traverse::enumerate(destination)
            .map_err(|error| self.io("enumerate destination for prune", b".", error))?
        {
            self.progress.record_scanned_entry();
            let destination_stamp =
                traverse::stat_child(destination, &entry.name, self.options.mount_policy)
                    .map_err(|error| self.io("stat destination for prune", &entry.name, error))?;
            let source_stamp =
                match traverse::stat_child(source, &entry.name, self.options.mount_policy) {
                    Ok(stamp) => Some(stamp),
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(error) => return Err(self.io("stat source for prune", &entry.name, error)),
                };
            match source_stamp {
                None => self.remove_destination_only(
                    source,
                    destination,
                    &entry.name,
                    destination_stamp,
                )?,
                Some(source_stamp) => {
                    if source_stamp.kind != destination_stamp.kind {
                        return Err(self.conflict(
                            "prune",
                            &entry.name,
                            "source and destination types changed during sync",
                        ));
                    }
                    if source_stamp.kind == traverse::EntryKind::Directory {
                        let source_child = traverse::open_child_directory(
                            source,
                            &entry.name,
                            self.options.mount_policy,
                        )
                        .map_err(|error| {
                            self.io("open source directory during prune", &entry.name, error)
                        })?;
                        let destination_child = traverse::open_child_directory(
                            destination,
                            &entry.name,
                            self.options.mount_policy,
                        )
                        .map_err(|error| {
                            self.io(
                                "open destination directory during prune",
                                &entry.name,
                                error,
                            )
                        })?;
                        self.prune_directory(&source_child, &destination_child)?;
                    }
                }
            }
        }
        let source_after = traverse::stamp_fd(source)
            .map_err(|error| self.io("stat source after prune", b".", error))?;
        if source_before != source_after {
            return Err(self.conflict("prune", b".", "source directory changed during sync prune"));
        }
        self.finalize_directory(source, destination, b".")
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

fn execute_directory_root(engine: &mut Engine, source: &Root, destination: &Root) -> Result<()> {
    let source = traverse::DirectoryFd::from_owned(source.open_directory("open source root")?)
        .map_err(|error| engine.io("open source root", b".", error))?;
    let destination = match destination.metadata().map(|metadata| metadata.kind) {
        Some(RootEntryKind::Directory) => Some(
            traverse::DirectoryFd::from_owned(destination.open_directory("open destination root")?)
                .map_err(|error| engine.io("open destination root", b".", error))?,
        ),
        Some(_) => {
            return Err(FsError::conflict(
                "converge root",
                &destination.input,
                "source and destination root types differ",
            ));
        }
        None if engine.options.dry_run => {
            engine.emit("mkdir", os_bytes(destination.leaf()));
            None
        }
        None => {
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
            Some(
                open_directory_from_root(destination, engine.options.mount_policy)
                    .map_err(|error| engine.io("open created destination root", b".", error))?,
            )
        }
    };
    // Keep a root-level Phase A stamp as well as the per-directory stamps in
    // `phase_a_directory`.  The second check below covers the narrow interval
    // after discovery and worker completion but before Phase B begins.
    let source_before_phase_a = if engine.options.operation == Operation::Sync {
        Some(
            traverse::stamp_fd(&source)
                .map_err(|error| engine.io("stat source root before Phase A", b".", error))?,
        )
    } else {
        None
    };

    engine.start_file_workers();
    engine.phase_a_directory(&source, destination.as_ref())?;
    engine.finish_file_workers()?;
    engine.progress.set_discovery_done(true);
    if let Some(destination) = destination.as_ref() {
        match engine.options.operation {
            Operation::Cp => {
                engine.finalize_deferred_directories()?;
                engine.finalize_directory(&source, destination, b".")?;
            }
            Operation::Sync => {
                let source_phase_a = source_before_phase_a
                    .expect("sync always captures a source root Phase A stamp");
                let source_before_prune = traverse::stamp_fd(&source).map_err(|error| {
                    engine.io("revalidate source root before prune", b".", error)
                })?;
                if source_phase_a != source_before_prune {
                    return Err(engine.conflict(
                        "sync",
                        b".",
                        "source root changed before sync prune",
                    ));
                }
                engine.progress.set_prune_active(true);
                let prune_result = engine.prune_directory(&source, destination);
                engine.progress.set_prune_active(false);
                prune_result?;
            }
        }
    }
    Ok(())
}

fn execute_regular_root(engine: &mut Engine, source: &Root, destination: &Root) -> Result<()> {
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
    let exists = destination.exists;
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
    let expected = copy::stamp_at(destination.parent_fd(), os_bytes(destination.leaf()))
        .map_err(|error| {
            engine.conflict(
                "stat destination root",
                os_bytes(destination.leaf()),
                error.to_string(),
            )
        })?
        .map(copy::DestinationExpectation::Present)
        .unwrap_or(copy::DestinationExpectation::Absent);
    copy::publish_regular_file_with_options(
        &source_fd,
        destination.parent_fd(),
        os_bytes(destination.leaf()),
        expected,
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
    let exists = destination.exists;
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
    let expected = copy::stamp_at(destination.parent_fd(), os_bytes(destination.leaf()))
        .map_err(|error| {
            engine.conflict(
                "stat destination root",
                os_bytes(destination.leaf()),
                error.to_string(),
            )
        })?
        .map(copy::DestinationExpectation::Present)
        .unwrap_or(copy::DestinationExpectation::Absent);
    copy::publish_symlink(
        source.parent_fd(),
        os_bytes(source.leaf()),
        destination.parent_fd(),
        os_bytes(destination.leaf()),
        expected,
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
}
