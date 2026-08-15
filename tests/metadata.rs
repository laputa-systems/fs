//! Integration coverage for metadata propagation and default no-op behavior.

use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs as rustix_fs;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
const TEST_XATTR: &[u8] = b"user.fs.integration";
#[cfg(target_os = "macos")]
const TEST_XATTR: &[u8] = b"com.fs.integration";

struct Fixture {
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::current_dir()
            .expect("workspace cwd")
            .join(format!(
                ".fs-metadata-test-{}-{nonce}-{counter}",
                std::process::id()
            ));
        fs::create_dir(&root).expect("create fixture");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn set_xattr(path: &std::path::Path, value: &[u8]) -> bool {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
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
    let source = fixture.root.join("source");
    let destination = fixture.root.join("destination");
    fs::create_dir(&source).expect("create source");
    let source_file = source.join("file");
    fs::write(&source_file, b"content").expect("write source");
    if !set_xattr(&source_file, b"source") {
        // The plan permits a capability skip only when the filesystem itself
        // lacks xattrs. This keeps unsupported temporary filesystems from
        // turning a portable semantic test into a false failure.
        return;
    }

    assert!(
        Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&fixture.root)
            .args(["cp", "--no-progress", "source", "destination"])
            .status()
            .expect("run cp")
            .success()
    );
    let destination_file = destination.join("file");
    assert_eq!(get_xattr(&destination_file), b"source");

    assert!(set_xattr(&destination_file, b"destination-only-drift"));
    assert!(
        Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&fixture.root)
            .args(["sync", "--no-progress", "source", "destination"])
            .status()
            .expect("run sync")
            .success()
    );
    assert_eq!(get_xattr(&destination_file), b"destination-only-drift");
}
