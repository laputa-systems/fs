//! Integration coverage for interruption and atomic publication.

use std::fs;
use std::time::Duration;

mod support;

use support::TestDir as Fixture;

#[test]
fn killing_an_inflight_copy_never_exposes_a_partial_final_file() {
    let fixture = Fixture::new();
    let source = fixture.root().join("source");
    let destination = fixture.root().join("destination");
    let old = vec![b'o'; 8 * 1024 * 1024];
    let new = vec![b'n'; 8 * 1024 * 1024];
    fs::write(&source, &new).expect("write source");
    fs::write(&destination, &old).expect("write destination");

    let mut child = fixture
        .command()
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
