//! Integration coverage for symlink and namespace replacement races.

mod support;

#[cfg(unix)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[cfg(unix)]
#[test]
fn root_components_never_follow_symlinks_but_final_source_link_is_an_object() {
    use std::fs;

    let fixture = support::TestDir::named("symlink-race");
    let root = fixture.root();
    fs::create_dir(root.join("real")).unwrap();
    fs::create_dir(root.join("real/source")).unwrap();
    fs::write(root.join("real/source/file"), b"payload").unwrap();
    std::os::unix::fs::symlink("real", root.join("alias")).unwrap();

    let rejected = fixture
        .command()
        .args(["cp", "alias/source", "destination"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!root.join("destination").exists());

    std::os::unix::fs::symlink("real/source/file", root.join("source-link")).unwrap();
    let copied = fixture
        .command()
        .args(["cp", "source-link", "destination-link"])
        .status()
        .unwrap();
    assert!(copied.success());
    assert_eq!(
        fs::read_link(root.join("destination-link")).unwrap(),
        std::path::Path::new("real/source/file")
    );

    fs::write(root.join("source-file"), b"source").unwrap();
    fs::write(root.join("outside"), b"outside").unwrap();
    std::os::unix::fs::symlink("outside", root.join("destination-link-conflict")).unwrap();
    let conflict = fixture
        .command()
        .args(["cp", "source-file", "destination-link-conflict"])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert_eq!(fs::read(root.join("outside")).unwrap(), b"outside");
}

/// A concurrent destination-directory replacement may make convergence fail,
/// but it must never turn a descendant lookup into a write through the link.
#[cfg(unix)]
#[test]
fn destination_directory_symlink_swaps_never_touch_the_outside_sentinel() {
    use std::fs;

    let fixture = support::TestDir::named("destination-symlink-swaps");
    let root = fixture.root().to_path_buf();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("source/volatile")).unwrap();
    for index in 0..256 {
        fs::write(
            root.join(format!("source/volatile/file-{index}")),
            b"source payload",
        )
        .unwrap();
    }
    fs::create_dir(root.join("destination")).unwrap();
    fs::create_dir(root.join("destination/volatile")).unwrap();
    fs::create_dir(root.join("outside")).unwrap();
    fs::write(
        root.join("outside/sentinel"),
        b"outside must remain unchanged",
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    let swap_root = root.clone();
    let swap_stop = stop.clone();
    let swap_count = swaps.clone();
    let swapper = std::thread::spawn(move || {
        let volatile = swap_root.join("destination/volatile");
        while !swap_stop.load(Ordering::Acquire) {
            if fs::remove_dir(&volatile).is_ok() {
                swap_count.fetch_add(1, Ordering::Relaxed);
                let _ = std::os::unix::fs::symlink("../outside", &volatile);
                let _ = fs::remove_file(&volatile);
                let _ = fs::create_dir(&volatile);
            }
            std::thread::yield_now();
        }
    });

    // Either convergence or a concurrent-change error is permitted; the
    // sentinel assertion below is the safety contract this adversarial run
    // exercises.
    let _output = fixture.run(&["cp", "--no-progress", "-j", "1", "source", "destination"]);
    stop.store(true, Ordering::Release);
    swapper.join().unwrap();

    assert!(
        swaps.load(Ordering::Relaxed) > 0,
        "the adversary did not successfully install a replacement"
    );
    assert_eq!(
        fs::read(root.join("outside/sentinel")).unwrap(),
        b"outside must remain unchanged"
    );
}

/// Source-side replacement is just as dangerous: a source child may vanish
/// or cause a conservative conflict, but a no-follow traversal must never
/// read data from the symlink target and publish it below the destination.
#[cfg(unix)]
#[test]
fn source_directory_symlink_swaps_never_copy_outside_data() {
    use std::fs;

    let fixture = support::TestDir::named("source-symlink-swaps");
    let root = fixture.root().to_path_buf();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("source/volatile")).unwrap();
    for index in 0..256 {
        fs::write(
            root.join(format!("source/volatile/file-{index}")),
            b"source payload",
        )
        .unwrap();
    }
    fs::create_dir(root.join("destination")).unwrap();
    fs::create_dir(root.join("destination/volatile")).unwrap();
    fs::create_dir(root.join("outside")).unwrap();
    fs::write(root.join("outside/secret"), b"outside data").unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    let swap_root = root.clone();
    let swap_stop = stop.clone();
    let swap_count = swaps.clone();
    let swapper = std::thread::spawn(move || {
        let volatile = swap_root.join("source/volatile");
        let parked = swap_root.join("source/.fs-test-parked");
        while !swap_stop.load(Ordering::Acquire) {
            if fs::rename(&volatile, &parked).is_ok() {
                swap_count.fetch_add(1, Ordering::Relaxed);
                let _ = std::os::unix::fs::symlink("../outside", &volatile);
                let _ = fs::remove_file(&volatile);
                let _ = fs::rename(&parked, &volatile);
            }
            std::thread::yield_now();
        }
        let _ = fs::remove_file(&volatile);
        if parked.exists() {
            let _ = fs::rename(&parked, &volatile);
        }
    });

    let _output = fixture.run(&["cp", "--no-progress", "-j", "1", "source", "destination"]);
    stop.store(true, Ordering::Release);
    swapper.join().unwrap();

    assert!(
        swaps.load(Ordering::Relaxed) > 0,
        "the adversary did not successfully replace the source component"
    );
    assert!(
        !root.join("destination/volatile/secret").exists(),
        "no-follow traversal must not copy the symlink target"
    );
    assert_eq!(
        fs::read(root.join("outside/secret")).unwrap(),
        b"outside data"
    );
}

#[cfg(target_os = "linux")]
mod linux_mounts {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct MountGuard(std::path::PathBuf);

    impl Drop for MountGuard {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }

    #[test]
    fn bind_mount_is_a_boundary_even_when_st_dev_matches() {
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let fixture = crate::support::TestDir::named(&format!("bind-mount-{nonce}"));
        let root = fixture.root();
        fs::create_dir(root.join("source")).unwrap();
        fs::create_dir(root.join("source/mounted")).unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/file"), b"mounted").unwrap();

        let mount_status = Command::new("mount")
            .args(["--bind"])
            .arg(root.join("outside"))
            .arg(root.join("source/mounted"))
            .status()
            .expect("invoke mount");
        if !mount_status.success() {
            // This is commonly unavailable in an unprivileged test runner.
            // The behavior is still covered wherever bind mounts are allowed.
            return;
        }
        let mount_guard = MountGuard(root.join("source/mounted"));

        let rejected = fixture
            .command()
            .args(["cp", "source", "destination"])
            .status()
            .expect("run fs without cross-filesystems");
        assert!(!rejected.success());

        let allowed = fixture
            .command()
            .args(["cp", "--cross-file-systems", "source", "destination"])
            .status()
            .expect("run fs with cross-filesystems");
        assert!(allowed.success());
        assert_eq!(
            fs::read(root.join("destination/mounted/file")).unwrap(),
            b"mounted"
        );

        drop(mount_guard);
    }
}
