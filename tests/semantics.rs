//! Integration coverage for the documented cp and sync semantics.

use std::fs;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

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
                ".fs-cli-test-{}-{nonce}-{counter}",
                std::process::id()
            ));
        fs::create_dir(&root).expect("create fixture");
        Self { root }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.join(name)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run fs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cp_overlays_while_sync_prunes_and_is_idempotent() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    fs::create_dir(fixture.path("source/nested")).unwrap();
    fs::write(fixture.path("source/file"), b"source").unwrap();
    fs::write(fixture.path("source/nested/file"), b"nested").unwrap();
    std::os::unix::fs::symlink("nested/file", fixture.path("source/link")).unwrap();

    fs::create_dir(fixture.path("destination")).unwrap();
    fs::write(fixture.path("destination/stale"), b"stale").unwrap();
    assert!(
        fixture
            .run(&["cp", "source", "destination"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination/file")).unwrap(),
        b"source"
    );
    assert!(fixture.path("destination/stale").exists());
    assert_eq!(
        fs::read_link(fixture.path("destination/link")).unwrap(),
        std::path::Path::new("nested/file")
    );

    assert!(
        fixture
            .run(&["sync", "source/", "destination/"])
            .status
            .success()
    );
    assert!(!fixture.path("destination/stale").exists());
    assert!(
        fixture
            .run(&["sync", "source", "destination"])
            .status
            .success()
    );
}

#[test]
fn absent_final_destination_is_valid_but_missing_parent_and_overlap_fail() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    fs::write(fixture.path("source/file"), b"source").unwrap();

    assert!(
        fixture
            .run(&["cp", "source", "new-destination"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("new-destination/file")).unwrap(),
        b"source"
    );

    let missing_parent = fixture.run(&["cp", "source", "missing/destination"]);
    assert!(!missing_parent.status.success());
    assert!(!fixture.path("missing").exists());

    let overlap = fixture.run(&["cp", "source", "source/new"]);
    assert!(!overlap.status.success());
    assert!(!fixture.path("source/new").exists());
}

#[test]
fn dry_run_never_creates_a_destination_and_type_conflicts_are_not_replaced() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    fs::write(fixture.path("source/file"), b"source").unwrap();

    assert!(
        fixture
            .run(&["cp", "--dry-run", "source", "absent"])
            .status
            .success()
    );
    assert!(!fixture.path("absent").exists());

    fs::write(fixture.path("conflict"), b"do not replace").unwrap();
    let conflict = fixture.run(&["cp", "source", "conflict"]);
    assert!(!conflict.status.success());
    assert_eq!(
        fs::read(fixture.path("conflict")).unwrap(),
        b"do not replace"
    );
}

#[test]
fn name_max_final_entry_does_not_break_short_temporary_names() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    let name = "n".repeat(255);
    fs::write(fixture.path("source").join(&name), b"payload").unwrap();

    assert!(
        fixture
            .run(&["cp", "source", "destination"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination").join(name)).unwrap(),
        b"payload"
    );
}

#[cfg(unix)]
#[test]
fn arbitrary_non_utf8_entry_names_are_preserved_without_path_reconstruction() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new();
    let source_name = OsString::from_vec(b"source-\xff".to_vec());
    let destination_name = OsString::from_vec(b"destination-\xfe".to_vec());
    let child_name = OsString::from_vec(b"child\n\t-\x80".to_vec());
    let source = fixture.root.join(&source_name);
    let destination = fixture.root.join(&destination_name);
    if let Err(error) = fs::create_dir(&source) {
        if error.raw_os_error() == Some(libc::EILSEQ) {
            // APFS volumes configured for Unicode normalization can reject
            // malformed UTF-8 at creation time. The command still retains
            // raw Unix bytes; run this assertion on filesystems that can
            // represent the test name instead of mistaking host policy for a
            // path-handling failure.
            return;
        }
        panic!("create non-UTF-8 source directory: {error}");
    }
    fs::write(source.join(&child_name), b"payload").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&fixture.root)
        .arg("cp")
        .arg("--no-progress")
        .arg(&source_name)
        .arg(&destination_name)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read(destination.join(child_name)).unwrap(), b"payload");
}
