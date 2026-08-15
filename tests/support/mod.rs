//! Dependency-free end-to-end test support.
//!
//! Every integration scenario gets an isolated directory below the host's
//! temporary area and invokes the compiled `fs` binary with that directory as
//! its working directory. Tests pass only relative operand paths to the
//! binary: this both mirrors ordinary CLI use and avoids accidentally testing
//! the host's `/var` compatibility symlink instead of `fs`'s component-wise
//! root resolver on macOS.

#![allow(dead_code)] // Each integration test compiles this shared module independently.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// A unique temporary directory removed when its test completes.
pub struct TestDir {
    root: PathBuf,
}

impl TestDir {
    /// Create a fresh fixture directory. Creation is exclusive and retried on
    /// the astronomically unlikely PID/time/counter collision.
    pub fn new() -> Self {
        Self::named("scenario")
    }

    /// Create a fresh fixture with a readable scenario label in its name.
    pub fn named(scenario: &str) -> Self {
        for _ in 0..32 {
            let counter = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock is after the Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "fs-e2e-{scenario}-{}-{nonce}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => return Self { root },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create e2e fixture {}: {error}", root.display()),
            }
        }
        panic!("could not create a unique e2e fixture directory")
    }

    /// Resolve a test-owned relative path.
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    /// The fixture root, for assertions about the test-owned namespace.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run the compiled binary from this isolated working directory.
    pub fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fs"))
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run fs end-to-end scenario")
    }

    /// Spawn the compiled binary from this isolated working directory.
    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fs"));
        command.current_dir(&self.root);
        command
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
