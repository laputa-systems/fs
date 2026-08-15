//! Integration coverage for the documented cp and sync semantics.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod support;

use support::TestDir as Fixture;

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

#[cfg(unix)]
#[test]
fn root_object_matrix_is_consistent_for_both_operations() {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ObjectKind {
        Regular,
        Directory,
        Symlink,
    }

    impl ObjectKind {
        const ALL: [Self; 3] = [Self::Regular, Self::Directory, Self::Symlink];

        fn label(self) -> &'static str {
            match self {
                Self::Regular => "file",
                Self::Directory => "directory",
                Self::Symlink => "symlink",
            }
        }
    }

    fn create(path: &std::path::Path, kind: ObjectKind, source: bool) {
        match kind {
            ObjectKind::Regular => {
                fs::write(
                    path,
                    if source {
                        b"source".as_slice()
                    } else {
                        b"destination".as_slice()
                    },
                )
                .unwrap();
            }
            ObjectKind::Directory => {
                fs::create_dir(path).unwrap();
                fs::write(
                    path.join(if source {
                        "source-child"
                    } else {
                        "stale-child"
                    }),
                    b"child",
                )
                .unwrap();
            }
            ObjectKind::Symlink => {
                std::os::unix::fs::symlink(
                    if source {
                        "source-target"
                    } else {
                        "destination-target"
                    },
                    path,
                )
                .unwrap();
            }
        }
    }

    for operation in ["cp", "sync"] {
        for source_kind in ObjectKind::ALL {
            for destination_kind in [
                None,
                Some(ObjectKind::Regular),
                Some(ObjectKind::Directory),
                Some(ObjectKind::Symlink),
            ] {
                let destination_label = destination_kind.map_or("absent", ObjectKind::label);
                let fixture = Fixture::named(&format!(
                    "matrix-{operation}-{}-{destination_label}",
                    source_kind.label()
                ));
                let source = fixture.path("source");
                let destination = fixture.path("destination");
                create(&source, source_kind, true);
                if let Some(destination_kind) = destination_kind {
                    create(&destination, destination_kind, false);
                }

                let output = fixture.run(&[operation, "--no-progress", "source", "destination"]);
                let compatible =
                    destination_kind.is_none() || destination_kind == Some(source_kind);
                assert_eq!(
                    output.status.success(),
                    compatible,
                    "{operation} {source_kind:?} -> {destination_kind:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if !compatible {
                    continue;
                }

                match source_kind {
                    ObjectKind::Regular => {
                        assert_eq!(fs::read(&destination).unwrap(), b"source");
                    }
                    ObjectKind::Directory => {
                        assert_eq!(
                            fs::read(destination.join("source-child")).unwrap(),
                            b"child"
                        );
                        if destination_kind == Some(ObjectKind::Directory) {
                            assert_eq!(
                                destination.join("stale-child").exists(),
                                operation == "cp",
                                "{operation} must {} destination-only children",
                                if operation == "cp" { "retain" } else { "prune" }
                            );
                        }
                    }
                    ObjectKind::Symlink => {
                        assert_eq!(
                            fs::read_link(&destination).unwrap(),
                            std::path::Path::new("source-target")
                        );
                    }
                }
            }
        }
    }
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

#[test]
fn printable_and_control_entry_names_are_copied_byte_for_byte() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    let names = [
        "space name",
        "tab\tname",
        "line\nname",
        "-leading-dash",
        "back\\slash",
        "unicøde",
    ];
    for (index, name) in names.iter().enumerate() {
        fs::write(
            fixture.path("source").join(name),
            format!("payload-{index}"),
        )
        .unwrap();
    }
    assert!(
        fixture
            .run(&["cp", "--no-progress", "source", "destination"])
            .status
            .success()
    );
    for (index, name) in names.iter().enumerate() {
        assert_eq!(
            fs::read_to_string(fixture.path("destination").join(name)).unwrap(),
            format!("payload-{index}")
        );
    }
}

#[cfg(unix)]
fn set_mtime(path: &std::path::Path, seconds: i64, nanoseconds: i64) {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open object for timestamp");
    rustix::fs::futimens(
        &file,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
        },
    )
    .expect("set timestamp");
}

#[cfg(unix)]
#[test]
fn semantic_matrix_covers_root_kinds_and_trailing_slashes() {
    let fixture = Fixture::new();

    // Absent sources fail without creating a destination.
    let absent = fixture.run(&["cp", "missing-source", "missing-destination"]);
    assert!(!absent.status.success());
    assert!(!fixture.path("missing-destination").exists());

    // A regular source can populate an absent regular destination and is
    // idempotent on an immediate second invocation.
    fs::write(fixture.path("source-file"), b"source-file").unwrap();
    assert!(
        fixture
            .run(&["cp", "source-file", "destination-file"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination-file")).unwrap(),
        b"source-file"
    );
    fs::write(fixture.path("destination-file"), b"sync-old").unwrap();
    assert!(
        fixture
            .run(&["sync", "source-file", "destination-file"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination-file")).unwrap(),
        b"source-file"
    );
    assert!(
        fixture
            .run(&["sync", "source-file", "destination-file"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["cp", "source-file", "destination-file"])
            .status
            .success()
    );

    // A changed regular destination is atomically replaced, while a type
    // conflict is rejected without replacing the existing object.
    fs::write(fixture.path("destination-file"), b"old").unwrap();
    assert!(
        fixture
            .run(&["cp", "source-file", "destination-file"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination-file")).unwrap(),
        b"source-file"
    );
    fs::create_dir(fixture.path("destination-dir-conflict")).unwrap();
    let conflict = fixture.run(&["cp", "source-file", "destination-dir-conflict"]);
    assert!(!conflict.status.success());
    assert!(fixture.path("destination-dir-conflict").is_dir());
    fs::write(fixture.path("outside"), b"outside").unwrap();
    std::os::unix::fs::symlink("outside", fixture.path("destination-link-conflict")).unwrap();
    let link_conflict = fixture.run(&["cp", "source-file", "destination-link-conflict"]);
    assert!(!link_conflict.status.success());
    assert_eq!(fs::read(fixture.path("outside")).unwrap(), b"outside");

    // A source directory overlays an existing directory and can also create
    // an absent destination directory.  cp retains stale entries; sync prunes
    // them, and the second sync is a no-op convergence check.
    fs::create_dir(fixture.path("source-dir")).unwrap();
    fs::write(fixture.path("source-dir/child"), b"child").unwrap();
    fs::create_dir(fixture.path("destination-dir")).unwrap();
    fs::write(fixture.path("destination-dir/stale"), b"stale").unwrap();
    assert!(
        fixture
            .run(&["cp", "source-dir/", "destination-dir/"])
            .status
            .success()
    );
    assert!(fixture.path("destination-dir/stale").exists());
    assert!(
        fixture
            .run(&["sync", "source-dir", "destination-dir"])
            .status
            .success()
    );
    assert!(!fixture.path("destination-dir/stale").exists());
    assert!(
        fixture
            .run(&["sync", "source-dir/", "destination-dir/"])
            .status
            .success()
    );

    // A directory source with trailing slash and one without it have the same
    // destination-root semantics.
    assert!(
        fixture
            .run(&["cp", "source-dir", "destination-dir-2"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["cp", "source-dir/", "destination-dir-3/"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.path("destination-dir-2/child")).unwrap(),
        fs::read(fixture.path("destination-dir-3/child")).unwrap()
    );

    // A final source symlink is an object, not a directory traversal.  An
    // existing destination symlink with the same target is also idempotent.
    std::os::unix::fs::symlink("source-file", fixture.path("source-link")).unwrap();
    assert!(
        fixture
            .run(&["cp", "source-link", "destination-link"])
            .status
            .success()
    );
    assert_eq!(
        fs::read_link(fixture.path("destination-link")).unwrap(),
        std::path::Path::new("source-file")
    );
    assert!(
        fixture
            .run(&["sync", "source-link", "destination-link"])
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn destination_intermediate_symlink_is_rejected_without_following_it() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    fs::write(fixture.path("source/file"), b"payload").unwrap();
    fs::create_dir(fixture.path("real-parent")).unwrap();
    std::os::unix::fs::symlink("real-parent", fixture.path("parent-link")).unwrap();

    let rejected = fixture.run(&["cp", "source", "parent-link/destination"]);
    assert!(!rejected.status.success());
    assert!(!fixture.path("real-parent/destination").exists());
}

#[test]
fn empty_source_distinguishes_cp_overlay_from_sync_prune() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("empty-source")).unwrap();
    fs::create_dir_all(fixture.path("destination/nested")).unwrap();
    fs::write(fixture.path("destination/stale"), b"stale").unwrap();
    fs::write(fixture.path("destination/nested/file"), b"stale").unwrap();

    assert!(
        fixture
            .run(&["cp", "empty-source", "destination"])
            .status
            .success()
    );
    assert!(fixture.path("destination/stale").exists());
    assert!(fixture.path("destination/nested/file").exists());
    assert!(
        fixture
            .run(&["sync", "empty-source", "destination"])
            .status
            .success()
    );
    assert!(!fixture.path("destination/stale").exists());
    assert!(!fixture.path("destination/nested").exists());
    assert!(
        fixture
            .run(&["sync", "empty-source", "destination"])
            .status
            .success()
    );
}

#[test]
fn dry_run_preserves_destination_bytes_metadata_and_private_namespace() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("source")).unwrap();
    fs::write(fixture.path("source/file"), b"new").unwrap();
    fs::create_dir(fixture.path("destination")).unwrap();
    fs::write(fixture.path("destination/file"), b"old").unwrap();
    fs::write(fixture.path("destination/stale"), b"stale").unwrap();
    #[cfg(unix)]
    fs::set_permissions(
        fixture.path("destination/file"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let before = fs::metadata(fixture.path("destination/file")).unwrap();
    let before_mode = {
        #[cfg(unix)]
        {
            before.permissions().mode()
        }
        #[cfg(not(unix))]
        {
            0
        }
    };
    let before_mtime = before.modified().unwrap();

    assert!(
        fixture
            .run(&[
                "sync",
                "--dry-run",
                "--no-progress",
                "source",
                "destination"
            ])
            .status
            .success()
    );
    assert_eq!(fs::read(fixture.path("destination/file")).unwrap(), b"old");
    assert!(fixture.path("destination/stale").exists());
    let after = fs::metadata(fixture.path("destination/file")).unwrap();
    #[cfg(unix)]
    assert_eq!(after.permissions().mode(), before_mode);
    assert_eq!(after.modified().unwrap(), before_mtime);
    let private = fs::read_dir(fixture.root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".fs."))
        .collect::<Vec<_>>();
    assert!(
        private.is_empty(),
        "dry-run left private entries: {private:?}"
    );

    assert!(
        fixture
            .run(&["sync", "--no-progress", "source", "destination"])
            .status
            .success()
    );
    assert_eq!(fs::read(fixture.path("destination/file")).unwrap(), b"new");
    assert!(!fixture.path("destination/stale").exists());
}

#[test]
fn dry_run_reports_the_complete_non_mutating_plan() {
    let fixture = Fixture::named("dry-run-output");
    fs::create_dir(fixture.path("source")).unwrap();
    fs::write(fixture.path("source/change"), b"new").unwrap();
    fs::create_dir(fixture.path("source/new-directory")).unwrap();
    fs::write(fixture.path("source/new-directory/child"), b"child").unwrap();
    fs::create_dir(fixture.path("destination")).unwrap();
    fs::write(fixture.path("destination/change"), b"old").unwrap();
    fs::write(fixture.path("destination/stale"), b"stale").unwrap();

    let output = fixture.run(&[
        "sync",
        "--dry-run",
        "--no-progress",
        "source",
        "destination",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout =
        String::from_utf8(output.stdout).expect("dry-run output is UTF-8 for this fixture");
    for expected in [
        "update\tchange",
        "mkdir\tnew-directory",
        "copy\tchild",
        "delete\tstale",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout:?}"
        );
    }
    assert_eq!(
        fs::read(fixture.path("destination/change")).unwrap(),
        b"old"
    );
    assert!(fixture.path("destination/stale").exists());
    assert!(!fixture.path("destination/new-directory").exists());
}

#[cfg(unix)]
#[test]
fn hash_mode_detects_same_size_same_mtime_content_drift() {
    let fixture = Fixture::new();
    fs::write(fixture.path("source"), b"new!").unwrap();
    fs::write(fixture.path("destination"), b"old!").unwrap();
    set_mtime(fixture.path("source").as_path(), 1_700_000_000, 0);
    set_mtime(fixture.path("destination").as_path(), 1_700_000_000, 0);

    // Metadata mode intentionally treats equal size and normalized mtime as
    // its fast equality proof, so content drift remains untouched here.
    assert!(
        fixture
            .run(&["cp", "--no-progress", "source", "destination"])
            .status
            .success()
    );
    assert_eq!(fs::read(fixture.path("destination")).unwrap(), b"old!");

    assert!(
        fixture
            .run(&[
                "cp",
                "--check=hash",
                "--no-progress",
                "source",
                "destination"
            ])
            .status
            .success()
    );
    assert_eq!(fs::read(fixture.path("destination")).unwrap(), b"new!");

    // Equal content with a different mtime is a hash-mode equality proof;
    // convergence updates metadata without recopying data.
    set_mtime(fixture.path("source").as_path(), 1_700_000_010, 0);
    set_mtime(fixture.path("destination").as_path(), 1_600_000_010, 0);
    assert!(
        fixture
            .run(&[
                "cp",
                "--check=hash",
                "--no-progress",
                "source",
                "destination"
            ])
            .status
            .success()
    );
    let source_mtime = fs::metadata(fixture.path("source"))
        .unwrap()
        .modified()
        .unwrap();
    let destination_mtime = fs::metadata(fixture.path("destination"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(destination_mtime, source_mtime);

    // Empty regular files still take the descriptor/hash path and remain
    // converged when their timestamps differ.
    fs::write(fixture.path("source"), b"").unwrap();
    fs::write(fixture.path("destination"), b"").unwrap();
    set_mtime(fixture.path("source").as_path(), 1_700_000_020, 0);
    set_mtime(fixture.path("destination").as_path(), 1_600_000_020, 0);
    assert!(
        fixture
            .run(&[
                "cp",
                "--check=hash",
                "--no-progress",
                "source",
                "destination"
            ])
            .status
            .success()
    );
    assert_eq!(fs::metadata(fixture.path("destination")).unwrap().len(), 0);
    assert!(
        fixture
            .run(&[
                "cp",
                "--check=hash",
                "--no-progress",
                "source",
                "destination"
            ])
            .status
            .success()
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
    let source = fixture.root().join(&source_name);
    let destination = fixture.root().join(&destination_name);
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

    let status = fixture
        .command()
        .arg("cp")
        .arg("--no-progress")
        .arg(&source_name)
        .arg(&destination_name)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read(destination.join(child_name)).unwrap(), b"payload");
}
