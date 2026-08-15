//! Deterministic end-to-end model testing for tree convergence.
//!
//! This intentionally uses a tiny local PRNG instead of adding `proptest`.
//! When a seed fails, the assertion identifies it and the exact generated
//! filesystem tree can be reproduced without external test infrastructure.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;

mod support;

use support::TestDir;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    Directory,
    File(Vec<u8>),
    Symlink(Vec<u8>),
}

type Tree = BTreeMap<String, Node>;

#[derive(Clone, Copy)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 7;
        self.0 ^= self.0 >> 9;
        self.0 ^= self.0 << 8;
        self.0
    }

    fn one_in(&mut self, denominator: u64) -> bool {
        self.next().is_multiple_of(denominator)
    }
}

fn generated_trees(seed: u64) -> (Tree, Tree) {
    let mut rng = Rng::new(seed);
    let mut source = Tree::new();
    let mut directories = Vec::new();
    for index in 0..3 {
        if !rng.one_in(3) {
            let name = format!("dir-{index}");
            source.insert(name.clone(), Node::Directory);
            directories.push(name);
        }
    }
    for index in 0..12 {
        let parent = if directories.is_empty() || rng.one_in(2) {
            String::new()
        } else {
            directories[(rng.next() as usize) % directories.len()].clone()
        };
        let path = if parent.is_empty() {
            format!("item-{index}")
        } else {
            format!("{parent}/item-{index}")
        };
        let node = if rng.one_in(4) {
            Node::Symlink(format!("target-{seed}-{index}").into_bytes())
        } else {
            Node::File(format!("source-{seed}-{index}-{}", rng.next()).into_bytes())
        };
        source.insert(path, node);
    }

    // Corresponding destination entries always retain type compatibility;
    // type conflicts have dedicated tests. Their data/link targets frequently
    // differ so convergence is exercised rather than only no-op comparison.
    let mut destination = Tree::new();
    for (path, node) in &source {
        if matches!(node, Node::Directory) {
            destination.insert(path.clone(), Node::Directory);
            continue;
        }
        if rng.one_in(3) {
            continue;
        }
        let destination_node = match node {
            Node::File(_) if rng.one_in(2) => {
                Node::File(format!("destination-{seed}-{path}").into_bytes())
            }
            Node::Symlink(_) if rng.one_in(2) => {
                Node::Symlink(format!("destination-target-{seed}").into_bytes())
            }
            node => node.clone(),
        };
        destination.insert(path.clone(), destination_node);
    }
    for index in 0..3 {
        let path = format!("extra-{index}");
        match rng.next() % 3 {
            0 => {
                destination.insert(path.clone(), Node::Directory);
                destination.insert(format!("{path}/child"), Node::File(b"extra child".to_vec()));
            }
            1 => {
                destination.insert(path, Node::File(b"extra file".to_vec()));
            }
            _ => {
                destination.insert(path, Node::Symlink(b"extra target".to_vec()));
            }
        }
    }
    (source, destination)
}

fn materialize(root: &std::path::Path, tree: &Tree) {
    fs::create_dir(root).expect("create tree root");
    for (path, node) in tree {
        if matches!(node, Node::Directory) {
            fs::create_dir(root.join(path)).expect("create modeled directory");
        }
    }
    for (path, node) in tree {
        match node {
            Node::Directory => {}
            Node::File(contents) => {
                fs::write(root.join(path), contents).expect("write modeled file");
            }
            Node::Symlink(target) => {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), root.join(path))
                    .expect("create modeled symlink");
            }
        }
    }
}

fn observe(root: &std::path::Path, relative: &str, observed: &mut Tree) {
    for entry in fs::read_dir(root).expect("enumerate modeled tree") {
        let entry = entry.expect("read modeled entry");
        let name = entry
            .file_name()
            .into_string()
            .expect("model names are ASCII");
        let path = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        let file_type = entry.file_type().expect("read modeled file type");
        if file_type.is_dir() {
            observed.insert(path.clone(), Node::Directory);
            observe(&entry.path(), &path, observed);
        } else if file_type.is_file() {
            observed.insert(
                path,
                Node::File(fs::read(entry.path()).expect("read modeled file")),
            );
        } else if file_type.is_symlink() {
            observed.insert(
                path,
                Node::Symlink(
                    fs::read_link(entry.path())
                        .expect("read modeled symlink")
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                ),
            );
        } else {
            panic!("model encountered unsupported filesystem object");
        }
    }
}

fn snapshot(root: &std::path::Path) -> Tree {
    let mut observed = Tree::new();
    observe(root, "", &mut observed);
    observed
}

#[test]
fn generated_trees_match_overlay_and_prune_models_then_are_idempotent() {
    // 128 seeds × both operations exercises 256 independently materialized
    // trees while keeping the binary-level test quick enough for local runs.
    for seed in 0..128_u64 {
        let (source_tree, destination_tree) = generated_trees(seed);
        for operation in ["cp", "sync"] {
            let fixture = TestDir::named(&format!("model-{operation}-{seed}"));
            let source = fixture.path("source");
            let destination = fixture.path("destination");
            materialize(&source, &source_tree);
            materialize(&destination, &destination_tree);

            let output = fixture.run(&[operation, "--no-progress", "source", "destination"]);
            assert!(
                output.status.success(),
                "seed {seed}, {operation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expected = if operation == "cp" {
                let mut overlay = destination_tree.clone();
                overlay.extend(source_tree.clone());
                overlay
            } else {
                source_tree.clone()
            };
            assert_eq!(
                snapshot(&destination),
                expected,
                "seed {seed}, {operation} diverged from its model"
            );

            let repeat = fixture.run(&[operation, "--no-progress", "source", "destination"]);
            assert!(
                repeat.status.success(),
                "seed {seed}, repeat {operation}: {}",
                String::from_utf8_lossy(&repeat.stderr)
            );
            assert_eq!(
                snapshot(&destination),
                expected,
                "seed {seed}, repeat {operation} was not idempotent"
            );
        }
    }
}
