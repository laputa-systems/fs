//! Integration coverage for bounded concurrency and atomic publication.

use std::fs;
use std::process::Command;
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
                ".fs-concurrency-test-{}-{nonce}-{counter}",
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

#[test]
fn concurrent_readers_observe_only_complete_old_or_new_publications() {
    let fixture = Fixture::new();
    let source = fixture.root.join("source");
    let destination = fixture.root.join("destination");
    let old = vec![b'o'; 4 * 1024 * 1024];
    let new = vec![b'n'; 4 * 1024 * 1024];
    fs::write(&source, &new).expect("write source");
    fs::write(&destination, &old).expect("write destination");

    let mut child = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&fixture.root)
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
    let source = fixture.root.join("source");
    let destination = fixture.root.join("destination");
    fs::create_dir(&source).expect("create source");
    for index in 0..128 {
        fs::write(
            source.join(format!("file-{index}")),
            format!("payload-{index}"),
        )
        .expect("write source entry");
    }

    let status = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&fixture.root)
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
