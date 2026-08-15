//! Integration coverage for metadata propagation and default no-op behavior.

use std::fs;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustix::fs as rustix_fs;

#[cfg(target_os = "linux")]
const TEST_XATTR: &[u8] = b"user.fs.integration";
#[cfg(target_os = "macos")]
const TEST_XATTR: &[u8] = b"com.fs.integration";

mod support;

use support::TestDir as Fixture;

fn set_xattr(path: &std::path::Path, value: &[u8]) -> bool {
    // Regular files are opened writable because some kernels require a
    // writable descriptor for fsetxattr. Directories generally cannot be
    // opened read/write, so retain a read-only fallback for directory xattrs.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path))
        .expect("open xattr object");
    rustix_fs::fsetxattr(&file, TEST_XATTR, value, rustix_fs::XattrFlags::empty()).is_ok()
}

fn get_xattr(path: &std::path::Path) -> Vec<u8> {
    let file = fs::File::open(path).expect("open xattr object");
    let mut buffer = vec![0; 1024];
    let length = rustix_fs::fgetxattr(&file, TEST_XATTR, &mut buffer).expect("read xattr");
    buffer.truncate(length);
    buffer
}

#[test]
fn creation_propagates_xattrs_but_default_sync_leaves_xattr_only_drift() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    fs::create_dir(&source).expect("create source");
    let source_file = source.join("file");
    fs::write(&source_file, b"content").expect("write source");
    if !set_xattr(&source, b"source-directory") || !set_xattr(&source_file, b"source") {
        // The plan permits a capability skip only when the filesystem itself
        // lacks xattrs. This keeps unsupported temporary filesystems from
        // turning a portable semantic test into a false failure.
        return;
    }

    assert!(
        fixture
            .command()
            .args(["cp", "--no-progress", "source", "destination"])
            .status()
            .expect("run cp")
            .success()
    );
    assert_eq!(get_xattr(&destination), b"source-directory");
    let destination_file = destination.join("file");
    assert_eq!(get_xattr(&destination_file), b"source");

    assert!(set_xattr(&destination_file, b"destination-only-drift"));
    assert!(
        fixture
            .command()
            .args(["sync", "--no-progress", "source", "destination"])
            .status()
            .expect("run sync")
            .success()
    );
    assert_eq!(get_xattr(&destination_file), b"destination-only-drift");
}

#[cfg(unix)]
fn set_mtime(path: &std::path::Path, seconds: i64, nanoseconds: i64) {
    let file = fs::File::open(path).expect("open object for timestamp");
    rustix_fs::futimens(
        &file,
        &rustix_fs::Timestamps {
            last_access: rustix_fs::Timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
            last_modification: rustix_fs::Timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
        },
    )
    .expect("set timestamp");
}

#[cfg(unix)]
#[test]
fn cp_and_sync_propagate_file_and_directory_modes_and_finalize_directory_mtime() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    fs::create_dir(&source).expect("create source");
    let source_file = source.join("file");
    fs::write(&source_file, b"content").expect("write source file");
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o640))
        .expect("set source file mode");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751))
        .expect("set source directory mode");
    set_mtime(&source_file, 1_700_000_100, 123_000_000);
    // Set this after creating all children: sync must apply it after its prune
    // traversal, because child creation and deletion both invalidate mtime.
    set_mtime(&source, 1_700_000_200, 456_000_000);

    assert!(
        fixture
            .command()
            .args(["cp", "--no-progress", "source", "destination"])
            .status()
            .expect("run cp")
            .success()
    );
    let copied_file = destination.join("file");
    let copied_file_stat = fs::metadata(&copied_file).expect("stat copied file");
    let copied_dir_stat = fs::metadata(&destination).expect("stat copied directory");
    assert_eq!(copied_file_stat.permissions().mode() & 0o7777, 0o640);
    assert_eq!(copied_dir_stat.permissions().mode() & 0o7777, 0o751);
    assert_eq!(copied_file_stat.mtime(), 1_700_000_100);
    assert_eq!(copied_file_stat.mtime_nsec(), 123_000_000);
    assert_eq!(copied_dir_stat.mtime(), 1_700_000_200);
    assert_eq!(copied_dir_stat.mtime_nsec(), 456_000_000);

    // Introduce a destination-only entry and a deliberately wrong directory
    // mode/mtime, then verify sync's prune traversal precedes finalization.
    fs::write(destination.join("stale"), b"stale").expect("write stale entry");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .expect("set destination mode");
    set_mtime(&destination, 1_700_000_300, 0);
    assert!(
        fixture
            .command()
            .args(["sync", "--no-progress", "source", "destination"])
            .status()
            .expect("run sync")
            .success()
    );
    let finalized = fs::metadata(&destination).expect("stat finalized directory");
    assert!(!destination.join("stale").exists());
    assert_eq!(finalized.permissions().mode() & 0o7777, 0o751);
    assert_eq!(finalized.mtime(), 1_700_000_200);
    assert_eq!(finalized.mtime_nsec(), 456_000_000);
}
