//! Integration coverage for interruption and atomic publication.

use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
                ".fs-interruption-test-{}-{nonce}-{counter}",
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
fn killing_an_inflight_copy_never_exposes_a_partial_final_file() {
    let fixture = Fixture::new();
    let source = fixture.root.join("source");
    let destination = fixture.root.join("destination");
    let old = vec![b'o'; 8 * 1024 * 1024];
    let new = vec![b'n'; 8 * 1024 * 1024];
    fs::write(&source, &new).expect("write source");
    fs::write(&destination, &old).expect("write destination");

    let mut child = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&fixture.root)
        .args(["cp", "--no-progress", "source", "destination"])
        .spawn()
        .expect("spawn fs");
    std::thread::sleep(Duration::from_millis(1));
    // The copy can legitimately have completed before the signal is
    // delivered, especially on a clone-capable filesystem. Either outcome is
    // valid; what matters is that the final name is never a partial file.
    let _ = child.kill();
    let _ = child.wait();

    let observed = fs::read(&destination).expect("read destination after interruption");
    assert!(
        observed == old || observed == new,
        "interruption exposed partial data"
    );
    // A forced process kill cannot run the temporary guard's cleanup. A
    // leftover private sibling is therefore allowed; the invariant is that it
    // never occupied the user-visible final name as a partial publication.
}
