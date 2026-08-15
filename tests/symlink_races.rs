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
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct MountGuard(std::path::PathBuf);

    impl Drop for MountGuard {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).output();
        }
    }

    /// Mount namespace tests require `CAP_SYS_ADMIN`, which ordinary local
    /// test processes commonly lack. Keep the opt-out explicit rather than
    /// silently treating an unavailable mount as coverage. Privileged Linux
    /// CI must set `FS_TEST_PRIVILEGED_MOUNTS=1`.
    fn privileged_mount_tests_enabled() -> bool {
        if std::env::var_os("FS_TEST_PRIVILEGED_MOUNTS").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            true
        } else {
            eprintln!(
                "skipping privileged mount test; set FS_TEST_PRIVILEGED_MOUNTS=1 in Linux CI"
            );
            false
        }
    }

    #[test]
    fn bind_mount_is_a_boundary_even_when_st_dev_matches() {
        if !privileged_mount_tests_enabled() {
            return;
        }
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
            .output()
            .expect("invoke mount");
        assert!(
            mount_status.status.success(),
            "privileged mount test was enabled but bind mount failed: {}",
            String::from_utf8_lossy(&mount_status.stderr)
        );
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

    #[test]
    fn sync_rejects_a_destination_only_bind_mount_before_pruning() {
        if !privileged_mount_tests_enabled() {
            return;
        }
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let fixture = crate::support::TestDir::named(&format!("destination-bind-mount-{nonce}"));
        let root = fixture.root();
        fs::create_dir(root.join("source")).unwrap();
        fs::create_dir(root.join("destination")).unwrap();
        fs::create_dir(root.join("destination/mounted")).unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/sentinel"), b"must not be pruned").unwrap();

        let mount_status = Command::new("mount")
            .args(["--bind"])
            .arg(root.join("outside"))
            .arg(root.join("destination/mounted"))
            .output()
            .expect("invoke mount");
        assert!(
            mount_status.status.success(),
            "privileged mount test was enabled but bind mount failed: {}",
            String::from_utf8_lossy(&mount_status.stderr)
        );
        let mount_guard = MountGuard(root.join("destination/mounted"));

        let rejected = fixture
            .command()
            .args(["sync", "source", "destination"])
            .status()
            .expect("run fs without cross-filesystems");
        assert!(!rejected.success());
        assert_eq!(
            fs::read(root.join("outside/sentinel")).unwrap(),
            b"must not be pruned"
        );

        drop(mount_guard);
    }

    #[test]
    fn copies_between_distinct_source_and_destination_filesystems() {
        if !privileged_mount_tests_enabled() {
            return;
        }
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let fixture = crate::support::TestDir::named(&format!("tmpfs-copy-{nonce}"));
        let root = fixture.root();
        fs::create_dir(root.join("source")).unwrap();
        let payload = vec![b'x'; 2 * 1024 * 1024];
        fs::write(root.join("source/payload"), &payload).unwrap();
        fs::create_dir(root.join("tmpfs")).unwrap();

        let mount_status = Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=16m", "tmpfs"])
            .arg(root.join("tmpfs"))
            .output()
            .expect("invoke tmpfs mount");
        assert!(
            mount_status.status.success(),
            "privileged mount test was enabled but tmpfs mount failed: {}",
            String::from_utf8_lossy(&mount_status.stderr)
        );
        let mount_guard = MountGuard(root.join("tmpfs"));
        assert_ne!(
            fs::metadata(root.join("source")).unwrap().dev(),
            fs::metadata(root.join("tmpfs")).unwrap().dev(),
            "tmpfs fixture must actually be a distinct filesystem"
        );

        let copied = fixture
            .command()
            .args([
                "cp",
                "source",
                root.join("tmpfs/destination").to_str().unwrap(),
            ])
            .status()
            .expect("copy across filesystems");
        assert!(copied.success());
        assert_eq!(
            fs::read(root.join("tmpfs/destination/payload")).unwrap(),
            payload
        );

        drop(mount_guard);
    }

    #[test]
    fn cross_filesystem_enospc_stops_sync_before_destination_prune() {
        if !privileged_mount_tests_enabled() {
            return;
        }
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let fixture = crate::support::TestDir::named(&format!("tmpfs-enospc-{nonce}"));
        let root = fixture.root();
        fs::create_dir(root.join("source")).unwrap();
        fs::write(root.join("source/payload"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
        fs::create_dir(root.join("tmpfs")).unwrap();

        let mount_status = Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=1m", "tmpfs"])
            .arg(root.join("tmpfs"))
            .output()
            .expect("invoke tmpfs mount");
        assert!(
            mount_status.status.success(),
            "privileged mount test was enabled but tmpfs mount failed: {}",
            String::from_utf8_lossy(&mount_status.stderr)
        );
        let mount_guard = MountGuard(root.join("tmpfs"));
        fs::create_dir(root.join("tmpfs/destination")).unwrap();
        fs::write(root.join("tmpfs/destination/stale"), b"retain on ENOSPC").unwrap();

        let failed = fixture
            .command()
            .args([
                "sync",
                "--no-progress",
                "source",
                root.join("tmpfs/destination").to_str().unwrap(),
            ])
            .status()
            .expect("sync into constrained tmpfs");
        assert!(!failed.success(), "the tmpfs copy must fail with ENOSPC");
        assert!(
            root.join("tmpfs/destination/stale").exists(),
            "a Phase A data failure must prevent destructive sync prune"
        );

        drop(mount_guard);
    }
}
