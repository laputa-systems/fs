//! Integration coverage for symlink and namespace replacement races.

#[cfg(unix)]
#[test]
fn root_components_never_follow_symlinks_but_final_source_link_is_an_object() {
    use std::fs;
    use std::process::Command;

    let root = std::env::current_dir()
        .expect("workspace cwd")
        .join(format!(".fs-symlink-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("real")).unwrap();
    fs::create_dir(root.join("real/source")).unwrap();
    fs::write(root.join("real/source/file"), b"payload").unwrap();
    std::os::unix::fs::symlink("real", root.join("alias")).unwrap();

    let rejected = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&root)
        .args(["cp", "alias/source", "destination"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!root.join("destination").exists());

    std::os::unix::fs::symlink("real/source/file", root.join("source-link")).unwrap();
    let copied = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&root)
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
    let conflict = Command::new(env!("CARGO_BIN_EXE_fs"))
        .current_dir(&root)
        .args(["cp", "source-file", "destination-link-conflict"])
        .status()
        .unwrap();
    assert!(!conflict.success());
    assert_eq!(fs::read(root.join("outside")).unwrap(), b"outside");
    fs::remove_dir_all(root).unwrap();
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
        let root = std::env::current_dir()
            .expect("workspace cwd")
            .join(format!(".fs-bind-mount-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
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
            fs::remove_dir_all(root).unwrap();
            return;
        }
        let mount_guard = MountGuard(root.join("source/mounted"));

        let rejected = Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&root)
            .args(["cp", "source", "destination"])
            .status()
            .expect("run fs without cross-filesystems");
        assert!(!rejected.success());

        let allowed = Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&root)
            .args(["cp", "--cross-file-systems", "source", "destination"])
            .status()
            .expect("run fs with cross-filesystems");
        assert!(allowed.success());
        assert_eq!(
            fs::read(root.join("destination/mounted/file")).unwrap(),
            b"mounted"
        );

        drop(mount_guard);
        fs::remove_dir_all(root).unwrap();
    }
}
