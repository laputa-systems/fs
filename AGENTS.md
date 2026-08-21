# Filesystem convergence design contract

This document records the design that the current implementation is expected to
preserve. It is the durable replacement for the original implementation plan.
Incomplete work belongs in [`TODO.md`](TODO.md), not here.

## Scope

`fs` is a small, stateless Unix filesystem-convergence tool. The supported
operations are:

```text
fs cp SRC DST
fs sync SRC DST
```

The implementation targets macOS and Linux. Linux release validation is limited
to ext4 and tmpfs. The project does not provide a Windows fallback.

The shipped dependency set is intentionally small: `rustix`, `libc`, `lexopt`,
`blake3`, and `rayon`.

## CLI contract

The supported options are:

```text
-n, --dry-run
-v, --verbose
-j, --jobs N
    --check=metadata|hash
    --durable
    --cross-file-systems
    --no-progress
-h, --help
-V, --version
```

Defaults are metadata checking, automatic bounded jobs, non-durable
publication, mount-boundary enforcement, quiet non-interactive output, and
interactive progress only when stderr is a terminal.

`--jobs` must be at least one. The automatic worker count is clamped to the
range 2–8. `--check=hash` is the explicit strong content-comparison mode.

## User-visible semantics

- `cp` overlays the source tree onto the destination and retains destination-
  only entries.
- `sync` converges the destination to the source and prunes destination-only
  entries, only after copy convergence succeeds.
- Trailing slashes have no semantic effect.
- Exactly one source and one destination are accepted.
- Directories always denote their tree roots; there is no implicit basename
  insertion.
- Every intermediate path component must already be a real directory. Only the
  final destination component may be absent.
- A final source symlink is an object and is copied as a symlink. Intermediate
  symlinks are rejected and symlinks are never traversed.
- Supported source objects are regular files, directories, and symlinks.
  Unsupported inode types and source/destination type conflicts fail without an
  implicit replacement.
- Copying a source into itself is a no-op; overlapping source and destination
  directory trees are rejected before mutation.
- `--dry-run` performs no filesystem mutation, creates no destination or
  temporary object, and reports the operations that a real run would perform.
- `--verbose` reports completed mutations on stdout. Progress and diagnostics
  use stderr.

The root grammar and FD-relative path handling live in `src/path.rs`; command
parsing and help text live in `src/cli.rs` and `src/main.rs`.

## Filesystem safety invariants

These invariants are part of the contract, not implementation details:

1. `src/path.rs` resolves supplied roots component-by-component from held
   directory descriptors without following intermediate links.
2. Recursive operations in `src/engine/traverse.rs`, `src/engine/sync.rs`, and
   `src/engine/delete.rs` use directory descriptors and single path components;
   they never reconstruct a multi-component path for mutation.
3. Directory-entry type bits are advisory. Entries are re-statted without
   following links before they are opened or mutated.
4. Final regular-file publication is an atomic same-directory rename of a
   private temporary object.
5. Planned source and destination identities are revalidated before copying,
   metadata mutation, replacement, and temporary cleanup.
6. `sync` pruning starts only after successful Phase A convergence and source
   root revalidation. It is not a snapshot transaction and does not roll back
   already completed individual mutations.
7. Recursive deletion first takes control of destination-only directories under
   private names, then removes only objects whose identity is still verified.
8. Default traversal rejects mount-instance crossings. Linux uses `openat2`
   with `RESOLVE_NO_XDEV` where available and `statx` mount IDs as the
   descriptor-relative fallback. `--cross-file-systems` is the explicit opt-out.
9. Xattrs are propagated when an object is created or otherwise mutated; the
   default no-op proof does not enumerate xattrs.
10. Queues, per-worker buffers, directory work records, and live directory FDs
    are bounded. Wide and deep trees must not require one resident record or FD
    per entry.
11. No persistent index, cache, manifest, sidecar, database, or daemon state is
    written.
12. Unsafe code is isolated to narrow platform wrappers and every unsafe block
    carries a local `SAFETY:` explanation. `#![deny(unsafe_op_in_unsafe_fn)]`
    remains enabled.

The engine-level summary of these invariants is maintained in
`src/engine/mod.rs`.

## Comparison and metadata policy

Default metadata mode compares regular-file size, permission bits, and a
destination-normalized modification time. Known filesystem timestamp grids are
handled by `src/metadata.rs`. Unknown timestamp resolution is conservative:
same-size files with differing raw mtimes receive a BLAKE3 content proof instead
of being recopied solely because their representation is ambiguous.

Hash mode always uses BLAKE3 content comparison. Equal content may still receive
mode and mtime convergence. A no-op regular file does not receive an xattr scan.

Creation and replacement propagate source mode, mtime, and readable xattrs.
Metadata-only changes propagate the applicable mode, mtime, and xattrs. Directory
metadata is finalized after recursive work; `sync` finalization follows prune.
Ownership, ACLs, exact xattr equality, sparse topology, and hard-link topology
are intentionally outside V1.

## Copy and durability paths

`src/engine/copy.rs` owns the publication transaction and uses the platform
boundary in `src/platform/mod.rs`:

- macOS attempts an FD-relative APFS clone and falls back to buffered copying;
- Linux attempts `FICLONE`, then `copy_file_range`, then a reusable buffered
  read/write loop;
- unsupported clone pairs are cached only for the current invocation;
- partial copies never become visible under the final destination name.

Normal mode promises atomic publication, not power-loss durability. With
`--durable`, temporary file data and metadata are synchronized before rename and
the parent directory is synchronized after namespace changes. macOS uses its
stronger full-sync primitive for regular files where available.

## Traversal and concurrency

`src/engine/sync.rs` performs directory convergence in bounded phases. Regular
file work is sent through a bounded, cancellable worker queue in `src/workers.rs`.
Workers own reusable scratch buffers and descriptor capabilities rather than
reopening attacker-controlled paths. Directory enumeration can spill records to
the anonymous spool in `src/platform/spool.rs` so memory and descriptor use stay
bounded for wide and deep trees.

`src/progress.rs` provides a dependency-free delayed terminal meter with scan,
copy, hash, and prune phases. It is disabled by `--no-progress` and never writes
to stdout.

## Non-goals

V1 does not implement remote hosts, SSH, a wire protocol, daemon/configuration
files, include/exclude or glob syntax, compression, bandwidth limiting, resume
databases, persistent hashes/indexes, snapshots, versioning, rollback, multiple
source arguments, basename insertion, trailing-slash semantics, symlink
following, implicit type replacement, ACL or ownership preservation, hard-link
graph preservation, exact sparse extent preservation, device files, sockets,
FIFOs, Windows support, a TUI, JSON output, or plugins.

## Required checks for code changes

Run the narrowest relevant check first, then broaden it as needed:

```text
cargo test --all-targets
docker build -t fs-alpine-test .
```

The Alpine Dockerfile is the Linux/musl test path. Do not weaken safety or
semantic tests to accommodate a platform; make platform assumptions explicit in
the relevant code, test, or [`TODO.md`](TODO.md) entry.

When a contract changes, update this file, the nearest tests, and the user-facing
help or error text together.

## Engineering workflow

Before editing, inspect the relevant definitions, callers, tests, and local
conventions. For bug fixes, preserve a focused regression test. Prefer precise
types, narrow interfaces, explicit state transitions, and reversible changes.
Run the narrowest useful compiler or test check and report what was verified.
Do not run formatters, linters, or pre-commit hooks on the user's behalf, and do
not push changes to a remote.
