//! Integration coverage for bounded concurrency and atomic publication.

use std::fs;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

mod support;

use support::TestDir as Fixture;

#[test]
fn concurrent_readers_observe_only_complete_old_or_new_publications() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    let old = vec![b'o'; 4 * 1024 * 1024];
    let new = vec![b'n'; 4 * 1024 * 1024];
    fs::write(&source, &new).expect("write source");
    fs::write(&destination, &old).expect("write destination");

    let mut child = fixture
        .command()
        .args(["cp", "--no-progress", "source", "destination"])
        .spawn()
        .expect("spawn fs");

    // A rename makes the final destination name switch in one namespace
    // mutation.  Poll it while the child is alive, then once more after it
    // exits, to guard the exact externally visible contract rather than an
    // implementation detail of the temporary file.
    loop {
        let observed = fs::read(&destination).expect("read destination while copying");
        assert!(
            observed == old || observed == new,
            "saw a partial publication"
        );
        if child.try_wait().expect("poll child").is_some() {
            break;
        }
    }
    assert!(child.wait().expect("wait child").success());
    assert_eq!(fs::read(destination).expect("read final destination"), new);
}

#[test]
fn bounded_workers_converge_many_regular_files() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    fs::create_dir(&source).expect("create source");
    for index in 0..128 {
        fs::write(
            source.join(format!("file-{index}")),
            format!("payload-{index}"),
        )
        .expect("write source entry");
    }

    let status = fixture
        .command()
        .args(["cp", "--no-progress", "-j", "4", "source", "destination"])
        .status()
        .expect("run fs");
    assert!(status.success());
    for index in 0..128 {
        assert_eq!(
            fs::read_to_string(destination.join(format!("file-{index}"))).expect("read output"),
            format!("payload-{index}")
        );
    }
}

/// Directory finalization must not retain one source/destination FD pair for
/// every directory in a wide tree.  A deliberately low child-process FD limit
/// turns that resource contract into an observable integration test without
/// constraining the test runner itself.
#[cfg(unix)]
#[test]
fn cp_finalizes_wide_directory_trees_with_bounded_file_descriptors() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    fs::create_dir(&source).expect("create source");
    for index in 0..96 {
        fs::create_dir(source.join(format!("directory-{index}"))).expect("create source child");
    }

    let mut command = fixture.command();
    command.args(["cp", "--no-progress", "-j", "1", "source", "destination"]);
    // SAFETY: `pre_exec` runs only in the spawned child before `exec`. The
    // closure performs the async-signal-safe `setrlimit` syscall and returns
    // an ordinary I/O error if the limit cannot be applied.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let status = command.status().expect("run fs with low fd limit");
    assert!(status.success(), "cp must not retain every directory FD");
    for index in 0..96 {
        assert!(
            fixture
                .path(format!("destination/directory-{index}"))
                .is_dir()
        );
    }
}

/// A deep tree must not turn recursion depth into an open-FD requirement.
/// Directory work is expected to be bounded independently of tree shape.
#[cfg(unix)]
#[test]
fn cp_converges_deep_directory_trees_with_bounded_file_descriptors() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    fs::create_dir(&source).expect("create source");
    let mut leaf = source.clone();
    for _ in 0..96 {
        leaf.push("nested");
        fs::create_dir(&leaf).expect("create nested directory");
    }
    fs::write(leaf.join("payload"), b"deep payload").expect("write deep payload");

    let mut command = fixture.command();
    command.args(["cp", "--no-progress", "-j", "1", "source", "destination"]);
    // SAFETY: as in the wide-tree regression above, this child-only closure
    // invokes just the async-signal-safe `setrlimit` syscall before `exec`.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let status = command.status().expect("run fs with low fd limit");
    assert!(
        status.success(),
        "cp must not retain directory FDs by depth"
    );
    assert_eq!(
        fs::read(fixture.path(format!("destination/{}", "nested/".repeat(96) + "payload")))
            .expect("read copied deep payload"),
        b"deep payload"
    );
}

/// A deep tree with a sibling at every level exercises the resumable branch
/// of the walker, not merely its single-child fast path.  The child process
/// has too few descriptors for an implementation that leaves every ancestor
/// directory open while it returns to enumerate those siblings.
#[cfg(unix)]
#[test]
fn cp_resumes_deep_branching_directory_trees_with_bounded_file_descriptors() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    fs::create_dir(&source).expect("create source");
    let mut leaf = source.clone();
    for _ in 0..96 {
        fs::create_dir(leaf.join("sibling")).expect("create sibling directory");
        leaf.push("nested");
        fs::create_dir(&leaf).expect("create nested directory");
    }
    fs::write(leaf.join("payload"), b"deep branching payload").expect("write deep payload");

    let mut command = fixture.command();
    command.args(["cp", "--no-progress", "-j", "1", "source", "destination"]);
    // SAFETY: as in the other descriptor regressions, this child-only closure
    // invokes just the async-signal-safe `setrlimit` syscall before `exec`.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let status = command.status().expect("run fs with low fd limit");
    assert!(
        status.success(),
        "cp must close ancestor descriptors before resuming siblings"
    );
    assert_eq!(
        fs::read(fixture.path(format!("destination/{}", "nested/".repeat(96) + "payload")))
            .expect("read copied deep payload"),
        b"deep branching payload"
    );
    for depth in 0..96 {
        assert!(
            fixture
                .path(format!("destination/{}sibling", "nested/".repeat(depth)))
                .is_dir(),
            "missing copied sibling at depth {depth}"
        );
    }
}

/// `sync` has a second, post-copy destination traversal. Its prune walk must
/// obey the same descriptor bound as Phase A while it descends and returns to
/// sibling directories.
#[cfg(unix)]
#[test]
fn sync_prunes_deep_branching_directory_trees_with_bounded_file_descriptors() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    fs::create_dir(&source).expect("create source");
    fs::create_dir(&destination).expect("create destination");
    let mut source_leaf = source.clone();
    let mut destination_leaf = destination.clone();
    for _ in 0..96 {
        fs::create_dir(source_leaf.join("sibling")).expect("create source sibling");
        fs::create_dir(destination_leaf.join("sibling")).expect("create destination sibling");
        source_leaf.push("nested");
        destination_leaf.push("nested");
        fs::create_dir(&source_leaf).expect("create source nested directory");
        fs::create_dir(&destination_leaf).expect("create destination nested directory");
    }
    fs::write(source_leaf.join("payload"), b"sync payload").expect("write source payload");
    fs::write(destination_leaf.join("payload"), b"sync payload")
        .expect("write destination payload");
    let stale = destination_leaf.join("stale");
    fs::write(&stale, b"remove me").expect("write stale destination entry");

    let mut command = fixture.command();
    command.args(["sync", "--no-progress", "-j", "1", "source", "destination"]);
    // SAFETY: this is the same child-only, async-signal-safe resource-limit
    // setup used by the `cp` descriptor regressions above.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let status = command.status().expect("run fs with low fd limit");
    assert!(
        status.success(),
        "sync prune must close ancestor descriptors before resuming siblings"
    );
    assert!(
        !stale.exists(),
        "sync must still prune the stale leaf entry"
    );
}

/// A private destination-only directory is also traversed recursively during
/// `sync` deletion. Its remover must not make the depth of stale content an
/// FD requirement.
#[cfg(unix)]
#[test]
fn sync_prunes_deep_destination_only_directories_with_bounded_file_descriptors() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    fs::create_dir(&source).expect("create source");
    fs::create_dir(&destination).expect("create destination");
    let mut stale = destination.join("stale");
    fs::create_dir(&stale).expect("create stale root");
    for _ in 0..96 {
        stale.push("nested");
        fs::create_dir(&stale).expect("create stale nested directory");
    }
    fs::write(stale.join("payload"), b"stale payload").expect("write stale payload");

    let mut command = fixture.command();
    command.args(["sync", "--no-progress", "-j", "1", "source", "destination"]);
    // SAFETY: this is the same child-only, async-signal-safe resource-limit
    // setup used by the other descriptor regressions in this module.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let status = command.status().expect("run fs with low fd limit");
    assert!(
        status.success(),
        "sync must not retain a descriptor for every stale directory"
    );
    assert!(
        !fixture.path("destination/stale").exists(),
        "sync must remove the stale directory tree"
    );
}
