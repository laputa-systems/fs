//! Integration coverage for symlink and namespace replacement races.

mod support;

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
