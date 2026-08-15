# Implementation Plan: Minimal, Safe, Extremely Fast Filesystem Convergence Tool

## 0. Mission

Implement a small Rust CLI for **local filesystem convergence**.

It replaces the common use of:

```bash
cp -r
rsync -a
rsync -a --delete
```

with two operations whose semantics are intentionally simpler:

```bash
fs cp SRC DST
fs sync SRC DST
```

The project is not an rsync clone and must never grow into one.

Its defining properties are:

1. **Idempotent**

   * Re-running a completed operation is a no-op.

2. **Stateless**

   * No database.
   * No cache directory.
   * No index.
   * No manifest.
   * No sidecar metadata.
   * No daemon.
   * Every invocation discovers current state directly from the filesystem.

3. **Race-resistant**

   * Once roots are established, filesystem operations are performed relative to directory file descriptors.
   * Never reconstruct absolute paths for mutation.
   * Symlinks are never followed during traversal.
   * Concurrent path replacement must not allow traversal outside the selected trees.

4. **Fast**

   * No pre-scan before useful work begins.
   * Metadata fast path for normal idempotent runs.
   * APFS clone fast path on macOS.
   * `FICLONE`/`copy_file_range` fast paths on Linux.
   * Bounded parallelism.
   * BLAKE3 for explicit strong comparison.
   * No unnecessary file reads.
   * No unnecessary allocations.
   * No async runtime.

5. **Minimal**

   * Very small CLI.
   * Very small dependency graph.
   * Unix only.
   * macOS is the primary platform.
   * Linux is a first-class platform.
   * No Windows support.

6. **Predictable**

   * No trailing-slash grammar.
   * No contextual destination semantics.
   * No multiple-source mode.
   * No implicit basename insertion.
   * No compatibility flags inherited from `cp` or `rsync`.

The implementation should feel more like `ripgrep` than rsync: a small binary that inspects the filesystem directly, does the obvious thing extremely quickly, and leaves no persistent machinery behind.

---

# 1. User-visible semantics

These semantics are contractual.

Do not silently change them during implementation.

## 1.1 `cp`

```bash
fs cp SRC DST
```

means:

> Converge everything represented by `SRC` into the corresponding object/tree at `DST`, while preserving destination entries that have no source counterpart.

For directories:

```text
SRC/
  a
  dir/
    b
```

then:

```bash
fs cp SRC DST
```

produces:

```text
DST/
  a
  dir/
    b
```

It does **not** produce:

```text
DST/
  SRC/
    a
```

Directories denote their trees/contents.

If `DST` already contains:

```text
DST/
  extra
```

then `extra` remains.

Conceptually:

```text
DST := overlay(DST, SRC)
```

## 1.2 `sync`

```bash
fs sync SRC DST
```

means:

> Make the destination tree correspond to the source tree, including removal of destination-only entries.

Conceptually:

```text
DST := SRC
```

subject to the deliberately documented metadata/non-goals below.

Deletion is a distinct verb because it is materially more dangerous than copy overlay.

Do not implement:

```bash
fs cp --delete
```

as the primary interface.

## 1.3 Trailing slashes have no semantics

These are identical:

```bash
fs cp src dst
fs cp src/ dst
fs cp src dst/
fs cp src/ dst/
```

Likewise for `sync`.

Strip or normalize redundant trailing separators before semantic interpretation.

The root path `/` remains `/`.

Never inspect whether the user supplied a trailing slash to determine behavior.

## 1.4 Destination existence does not change meaning

These must have the same semantics whether `DST` already exists or not:

```bash
fs cp src dst
```

There must be no GNU `cp`-style contextual basename insertion.

If `src` is a directory, `dst` denotes the destination root.

If `src` is a regular file, `dst` denotes the destination file.

If the user wants:

```text
dst/src
```

they must write:

```bash
fs cp src dst/src
```

### Root-path resolution

There is no implicit `mkdir -p` behavior.

Every non-final component of both supplied paths must already exist and be a
real directory. Only the final `DST` component may be absent. Therefore:

```bash
fs cp src nonexistent-parent/dst
```

is an error, while:

```bash
fs cp src existing-parent/new-dst
```

is valid.

Resolve each supplied path component-by-component from a held directory FD:

* do not follow symbolic links in intermediate components;
* inspect the final `SRC` component without following it, so a source symlink
  is copied as a symlink object;
* inspect the final `DST` component without following it, whether it exists or
  is absent.

`canonicalize()` is not part of the safety model. It follows links, cannot
represent an absent final destination, and creates the wrong race boundary.

## 1.5 Exactly one source and destination

Support:

```bash
fs cp SRC DST
fs sync SRC DST
```

Do not support:

```bash
fs cp A B C DST
```

This removes a large class of contextual semantics.

## 1.6 Supported source object types

V1 source objects:

* regular files
* directories
* symbolic links

Unsupported source objects:

* sockets
* FIFOs
* character devices
* block devices
* other unusual inode types

Encountering an unsupported source entry is an error.

Do not silently ignore it.

## 1.7 Symbolic links

Symlinks are **objects**, never traversal directives.

Given:

```text
SRC/current -> releases/42
```

produce the same symlink under `DST`.

Never follow symlinks while recursively traversing.

Do not implement `-L`, `-H`, `-P`, or similar compatibility modes in V1.

A symlink supplied as the root `SRC` is also copied as a symlink rather than followed as a directory.

This rule must be uniform.

## 1.8 Type conflicts

If corresponding source and destination names have different object types, fail.

Examples:

```text
source foo = directory
destination foo = regular file
```

Error.

```text
source foo = regular file
destination foo = symlink
```

Error.

```text
source foo = symlink
destination foo = directory
```

Error.

Do **not** recursively destroy an existing object merely because the source has a different type.

Do not implement `--replace` in V1.

This distinction is important:

> Replacing the contents of an existing regular file is ordinary convergence. Changing an object's filesystem type is destructive and suspicious.

## 1.9 Self-copy

If source and destination resolve to the exact same filesystem object:

```text
same st_dev + st_ino
```

treat the operation as an immediate successful no-op.

If one directory tree is an ancestor of the other, reject the operation before mutation.

Examples:

```bash
fs cp /a /a/b
fs sync /a/b /a
```

must fail.

Do not attempt to special-case traversal around this.

---

# 2. CLI

Keep the user-facing CLI intentionally tiny.

Target:

```text
fs cp [OPTIONS] SRC DST
fs sync [OPTIONS] SRC DST

Options:
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

Do not add more flags without a concrete requirement.

The root-resolution, timestamp, xattr, mount-boundary, cleanup, and live-source
rules in this document are internal correctness contracts. They do not justify
new user-facing compatibility or implementation-detail flags.

## 2.1 Defaults

```text
operation       cp/sync verb
check           metadata
jobs            auto
durable         false
cross-filesystems false
progress        auto when stderr is a TTY
verbose         false
dry-run         false
```

### Important mount-instance policy

By default, **do not traverse into a distinct mounted filesystem or mount
instance beneath either selected root**. This is deliberately stronger than a
mere `st_dev` device check: on Linux it includes bind mounts.

Inspect every child entry for a boundary before operating on it. A prohibited
boundary is an error regardless of the child type; never silently skip an
entry or only reject directories after beginning traversal.

Crossing requires:

```bash
--cross-file-systems
```

This is intentionally safer than conventional recursive copy behavior.

It prevents a destructive `sync` from accidentally descending into and emptying a mounted external filesystem.

## 2.2 `--dry-run`

`--dry-run` performs zero filesystem mutations.

It prints every operation that would be performed.

Example:

```text
mkdir   src
copy    src/a
update  src/b
delete  stale
```

Dry-run must still perform enough metadata inspection to generate an accurate plan.

Do not create temporary files.

Do not create destination directories.

Do not modify metadata.

## 2.3 `--verbose`

Normal successful operation is otherwise quiet apart from an interactive progress meter.

`--verbose` emits completed mutations.

Output order need not be deterministic because execution is concurrent.

Verbose output goes to stdout.

Progress and diagnostics go to stderr.

## 2.4 `--jobs`

Accept:

```bash
-j 1
-j 8
--jobs 8
```

Default is an automatically selected bounded worker count.

Do not expose separate walker/hash/copy worker knobs.

One expert concurrency knob is enough.

Validate:

```text
N >= 1
```

The initial auto heuristic should be conservative:

```text
available = std::thread::available_parallelism()
jobs = clamp(available, 2, 8)
```

Treat that only as the starting heuristic.

Benchmark macOS APFS and Linux filesystems and tune before release.

---

# 3. Dependency budget

Prefer this dependency graph:

```toml
rustix
libc
lexopt
blake3
rayon
```

`rayon` is justified because explicit BLAKE3 hashing can exploit parallel CPU execution.

`libc` is justified for Darwin-specific interfaces not wrapped by rustix, especially clone/copy/durability primitives.

Do not add:

* clap
* anyhow
* thiserror
* tokio
* async-std
* walkdir
* tempfile
* indicatif
* console
* serde
* serde_json
* crossbeam
* dashmap
* tracing
* regex

unless a later requirement demonstrates that implementing the tiny needed subset locally would be materially worse.

`rustix` already exposes the core fd-relative primitives needed here, including `openat`, `statat`, `renameat`, `unlinkat`, `fstat`, xattrs, timestamps, `copy_file_range`, Linux `FICLONE`, and synchronization operations.

The progress UI can use `rustix::termios::isatty` and `tcgetwinsize` directly, avoiding a terminal UI dependency.

---

# 4. Project structure

Keep module boundaries aligned with actual responsibilities.

Suggested layout:

```text
src/
  main.rs
  cli.rs
  error.rs
  path.rs

  engine/
    mod.rs
    copy.rs
    sync.rs
    traverse.rs
    compare.rs
    delete.rs

  platform/
    mod.rs
    macos.rs
    linux.rs

  metadata.rs
  hash.rs
  workers.rs
  progress.rs

tests/
  semantics.rs
  symlink_races.rs
  interruption.rs
  metadata.rs
  concurrency.rs

scripts/
  bench.sh
```

Do not create elaborate abstraction layers merely to support hypothetical future operating systems.

`platform::{macos,linux}` may explicitly contain different optimized implementations behind a small internal interface.

---

# 5. Fundamental filesystem model

This is one of the most important parts of the project.

## 5.1 Paths are only for locating roots

User paths are used only to establish the source and destination roots, but
that establishment is itself FD-relative and component-by-component. Start at
an FD for `/` for an absolute path or the current working directory for a
relative path. Open every non-final component with directory and no-follow
requirements, then retain the final parent FD plus final component name.

This means a missing intermediate destination component, an intermediate
symlink, or an intermediate non-directory is an invocation error. The only
lookup allowed to observe absence is the final destination component.

Once the root objects/directories are established:

> Do not perform recursive operations using reconstructed absolute or multi-component paths.

Every traversal step should work relative to an already-open directory descriptor.

## 5.2 Child operations use one path component

Inside a directory:

```text
parent_fd + child_name
```

is the unit of lookup.

Never do:

```text
openat(root_fd, "foo/bar/baz", ...)
```

during traversal.

Instead:

```text
foo_fd = openat(root_fd, "foo")
bar_fd = openat(foo_fd, "bar")
baz_fd = openat(bar_fd, "baz")
```

This is especially important on macOS, which lacks Linux's full `openat2` resolution model.

A single-component lookup relative to a trusted directory FD plus no-follow flags gives far stronger invariants.

## 5.3 Never trust `readdir` type information as identity

Directory enumeration may tell us an entry appears to be a regular file, directory, etc.

That information is advisory.

Before acting:

1. `statat(..., NOFOLLOW)`
2. open the entry using the expected type and `O_NOFOLLOW`
3. `fstat()` the resulting FD
4. verify that the opened object matches the previously observed identity

Use at minimum:

```text
st_dev
st_ino
file type
```

For mutation conflict checks also track:

```text
size
mtime
ctime
mode
```

Call this structure something like:

```rust
FileStamp
```

Do not use path lookup followed later by an unchecked mutation.

## 5.4 Directory opening

Conceptually:

```text
openat(
    parent,
    child,
    O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC
)
```

Then immediately `fstat`.

On Linux, use `openat2` where useful and available, but correctness must not depend on it because macOS is a primary target.

Linux can additionally request strong resolution restrictions such as beneath/no-symlink behavior.

## 5.5 Regular file opening

Open source regular files with:

```text
O_RDONLY | O_NOFOLLOW | O_CLOEXEC
```

Then verify with `fstat`.

Never copy by reopening an absolute pathname later.

## 5.6 Destination mutation

Every mutation occurs relative to an already-open destination parent FD:

* `mkdirat`
* `renameat`
* `unlinkat`
* `symlinkat`
* `openat`
* fd metadata operations

The mutation layer should not accept arbitrary `PathBuf`s.

Design its APIs so unsafe pathname behavior is structurally difficult to express.

---

# 6. Root representation

Do not immediately canonicalize everything into strings.

Represent an operation root as either:

```text
(parent_dir_fd, leaf_name)
```

or an already-open root directory/file object.

For an absent destination, retain the established `parent_dir_fd` and its
single-component `leaf_name`; do not try to synthesize a destination root.
The parent is guaranteed to exist because root resolution never creates
intermediate components.

This supports:

* absent destination roots
* symlink roots
* exact root-object semantics
* fd-based identity checks

Do not depend on `canonicalize()` for safety.

Canonicalization is itself path-based, cannot represent an absent destination, and creates race windows.

---

# 7. Ancestor/overlap detection

Before any mutation:

1. establish source root identity
2. establish destination root identity when it exists
3. detect exact identity
4. detect directory ancestor relationships

For directories, perform ancestor detection from open descriptors rather than string prefixes.

Conceptually:

```text
current = dst_fd

loop:
    if identity(current) == identity(src):
        overlap
    parent = openat(current, "..")
    if identity(parent) == identity(current):
        reached filesystem root
    current = parent
```

When destination exists, open its root directory and perform the full
bidirectional ancestry check. When a destination directory is absent, compare
the source directory identity against the ancestry of the retained destination
parent FD. If the source appears there, the final destination leaf would be
created inside the source and the operation must fail. This rejects, for
example:

```bash
fs cp /a /a/b
```

without requiring the absent `b` to have an identity.

Perform both directions as necessary.

Reject nested source/destination trees before mutation.

A path string test such as:

```text
dst.starts_with(src)
```

is insufficient and must not be used as the security check.

---

# 8. Comparison semantics

## 8.1 Default: `--check=metadata`

The default exists for speed.

For corresponding regular files, content is considered unchanged when:

```text
same size
AND
equivalent mtime after normalization to the known destination timestamp resolution
```

`TimestampResolution` is an explicit per-destination-mount value. Prefer an
FD-based [`_PC_TIMESTAMP_RESOLUTION` query](https://man7.org/linux/man-pages/man3/fpathconf.3p.html)
where the platform exposes it; POSIX defines the value in nanoseconds and
allows it to vary by filesystem. Cache it only for this invocation, keyed by the destination mount identity. Platform
implementations for APFS and mainstream Linux filesystems must provide
deterministic normalization covered by tests.

Normalize both timestamps to that representable resolution before comparing;
do not use arbitrary “within one/two seconds” tolerances. This prevents a
filesystem with coarse timestamps from causing an otherwise converged file to
be recopied forever.

When destination resolution cannot be determined reliably, a same-size mtime
mismatch is *ambiguous*, not proof that data differs. Hash the open source and
destination FDs in that narrow case:

* different digests -> data differs and the file is replaced;
* equal digests -> data is converged and must not be recopied merely because
  the timestamp cannot be represented exactly.

For this ambiguous equal-content case, do not repeatedly apply an mtime-only
update. Mtime preservation on an unknown-resolution filesystem is best-effort;
it is not an excuse for endless metadata mutation.

This makes repeated no-op invocations primarily directory enumeration + metadata syscalls.

Metadata convergence is considered separately.

If size differs, treat file data as changed. A normalized mtime difference is
also proof of difference when the destination resolution is known.

This deliberately permits the theoretical case where two different files have identical size and mtime.

That tradeoff is explicit.

Users requiring content proof have:

```bash
--check=hash
```

## 8.2 `--check=hash`

Strong comparison uses BLAKE3.

Rules:

1. Different sizes → different immediately.
2. Same size → hash both open file descriptors.
3. Equal BLAKE3 digest → contents equal.
4. Different digest → replace destination.

Do not trust mtime for content equality in hash mode.

No hashes are persisted anywhere.

Every invocation remains stateless.

BLAKE3 is deliberately appropriate here because the algorithm is tree-structured, SIMD-friendly and highly parallelizable; its Rust implementation supports Rayon-backed parallel updates. Its own documentation notes that multithreaded hashing has overhead on smaller buffers/files, so the implementation must use a size threshold rather than blindly parallelizing every hash.

## 8.3 No discretionary hashing heuristics

Do not silently decide to hash arbitrary metadata mismatches. The
unknown-timestamp-resolution fallback above is a correctness rule, not a
performance heuristic.

For the default mode:

```text
known resolution + metadata mismatch => update/copy
unknown resolution + same-size mtime mismatch => BLAKE3 proof
```

For hash mode:

```text
content hash determines content equality
```

Keep the model obvious.

On APFS particularly, hashing a multi-gigabyte file merely to avoid an inexpensive CoW clone can be substantially worse than replacing it.

---

# 9. Hash implementation

Do not use path-based BLAKE3 mmap helpers because the core engine deliberately works with already-open descriptors.

Also avoid mmap as the primary hash implementation.

A concurrently truncated mmap can cause fatal mapping faults; ordinary `read`/`pread` gives controllable I/O errors instead.

Implement:

```text
hash_fd(fd, size, scratch)
```

using a reusable large worker buffer.

Initial candidate:

```text
1–8 MiB per buffer
```

Benchmark.

For small/medium files:

```text
read chunk
hasher.update(chunk)
```

BLAKE3 already uses SIMD internally.

For sufficiently large in-memory chunks, investigate:

```text
hasher.update_rayon(chunk)
```

The official implementation explicitly warns that `update_rayon` is slower for small inputs and recommends benchmarking the threshold.

Do not assume the documented x86 threshold applies to Apple Silicon.

Benchmark on Apple M-series hardware.

The hash engine must avoid nested uncontrolled thread creation.

Use one Rayon pool for hash compute.

Never spawn a new pool per file.

---

# 10. Copy architecture

Copying a changed regular file must be an atomic publication operation.

Never write directly into the existing destination file.

## 10.1 Generic replacement transaction

For an existing or absent destination file:

1. safely open source FD
2. capture source `FileStamp`
3. capture expected destination stamp or expected absence
4. construct a unique temporary sibling name
5. create/clone/copy into the temporary destination
6. apply required metadata
7. re-stat source FD
8. verify source remained stable
9. re-stat destination pathname and confirm it still matches the state observed during planning
10. optionally perform durability synchronization
11. `renameat(temp, final)`
12. optionally sync destination parent directory
13. report completion

Readers must see either:

```text
old destination
```

or:

```text
complete new destination
```

Never a partially-written destination.

## 10.2 Temporary names

Do not add a randomness dependency.

Use:

```text
.fs.tmp.<pid>.<atomic_counter>
.fs.rm.<pid>.<atomic_counter>
```

The names are deliberately independent of the user basename, so they remain
valid beside a `NAME_MAX`-length entry. Use `.fs.tmp` for unpublished files or
symlinks and `.fs.rm` for directories renamed aside during prune.

Creation must always use exclusive semantics. If collision occurs, increment
the counter and retry. Never overwrite a pre-existing temporary-looking file.

Every temporary guard records:

```text
parent_fd
temp_name
expected dev
expected ino
expected type
```

Before cleanup, unlink, or `rmdir`, use `statat(..., NOFOLLOW)` and verify that
the name still has this exact identity. If it does not, leave it behind and
report a cleanup conflict; never guess which object to remove.

The RAII guard performs this identity-checked cleanup on ordinary error paths.

This minimizes but cannot eliminate the residual POSIX namespace race where a
separate process replaces the name between revalidation and `unlinkat` or
`rmdir`. The invariant is therefore: never follow an attacker-controlled
replacement and never recursively operate through an unverified replacement.

A hard process crash may leave a stale temp file. That is acceptable in V1.

Never automatically delete old temp-looking files from previous invocations; ownership cannot be established safely.

---

# 11. macOS data-copy fast path

macOS is the priority platform.

The first goal on APFS is **not to copy bytes at all**.

Apple's APFS copy APIs support clone semantics, allowing files to share underlying data blocks through copy-on-write rather than physically duplicating the file immediately.

Use an open source FD and a destination-directory-relative clone operation where available, preferably the `fclonefileat` family exposed by Darwin/libc.

Target strategy:

```text
source FD
   |
   +--> APFS clone into temporary destination name
           success -> metadata/finalize -> atomic rename
           unsupported/cross-device -> fallback
```

Do not use a path-based clone if doing so would weaken the fd-relative source guarantees.

Cache structural clone failures **in memory for this invocation** keyed by filesystem pair where sensible, so thousands of files do not each pay an obviously unsupported clone syscall.

This cache is ephemeral process state, not a persistent index.

Only cache clearly structural errors such as:

```text
EXDEV
ENOTSUP / EOPNOTSUPP
```

Do not cache ambiguous per-file failures.

## 11.1 macOS fallback

If cloning cannot be used:

1. create temp regular file
2. consider `fcopyfile` for efficient data/metadata transfer
3. otherwise use an explicit buffered FD copy loop

Apple's `fcopyfile` operates on already-open file descriptors and can copy file data and selected metadata, including extended attributes.

Do not sacrifice cross-platform semantic consistency merely because `fcopyfile` supports more metadata than V1 promises.

---

# 12. Linux data-copy fast path

For Linux regular files:

### Fast path 1

Create the temporary destination file and attempt:

```text
FICLONE
```

via rustix:

```text
ioctl_ficlone(dst_fd, src_fd)
```

If supported, this provides filesystem reflink semantics.

rustix exposes this operation directly.

### Fast path 2

If reflink is unavailable, use:

```text
copy_file_range
```

in a loop until source EOF.

Linux filesystem implementations can provide optimized in-kernel copy behavior through this interface.

### Fast path 3

Fallback to buffered user-space read/write.

Do not build an elaborate splice/sendfile decision tree unless benchmarks demonstrate a measurable benefit.

---

# 13. Buffered-copy fallback

Each file-copy worker owns and reuses one scratch buffer.

Do not allocate a large buffer per file.

Initial range to benchmark:

```text
256 KiB
1 MiB
4 MiB
8 MiB
```

The selected size should optimize modern NVMe/APFS/ext4 workloads without causing silly memory use at `-j 8`.

The loop must correctly handle:

* short reads
* short writes
* EINTR
* EOF
* source truncation
* ENOSPC
* I/O errors

Do not use byte-at-a-time abstractions.

---

# 14. Source mutation detection

An open FD prevents path substitution, but it does not prevent the underlying file from being modified.

For every copied regular file:

Before copying capture:

```text
dev
ino
size
mtime
ctime
type
```

After copying, `fstat(source_fd)` again.

Require:

```text
same dev
same ino
same type
same size
same mtime
same ctime
```

If this changes:

1. discard destination temp
2. report `source changed during operation`
3. cancel further mutation
4. do not enter sync prune phase

Do not publish a possibly inconsistent copy.

A malicious actor with the ability to rewrite a file and restore all observable metadata can defeat metadata-based change detection; snapshot isolation is outside V1 scope.

---

# 15. Destination race protection

Never blindly replace a destination that changed after planning.

When comparison discovers an existing destination, capture a `FileStamp`.

Immediately before publishing the replacement:

```text
statat(dst_parent, dst_name, NOFOLLOW)
```

and compare against the expected stamp.

If the destination:

* disappeared unexpectedly
* appeared unexpectedly
* changed inode
* changed type
* changed size/mtime/ctime

treat that as a concurrent conflict.

Do not overwrite it.

Remove the temporary file and fail.

This prevents the tool from clobbering a third party's concurrent destination update merely because a worker happened to arrive later.

---

# 16. Directory traversal architecture

Do not use `walkdir`.

Build the traversal directly around directory descriptors.

## 16.1 Directory context

Conceptually:

```rust
struct DirContext {
    src: OwnedFd,
    dst: Option<OwnedFd>,
    relative_display_path: ...,
    root_device: Dev,
}
```

Use `Arc<DirContext>` where file tasks must keep the parent descriptors alive.

Do not duplicate a parent FD for every child file.

One directory FD pair can service many child operations.

## 16.2 Bounded outstanding work

Do not recursively open the entire tree and retain one FD per directory.

Use bounded work queues.

Scanning a directory should:

1. enumerate immediate children
2. open child directories safely
3. enqueue bounded child directory work
4. close no-longer-needed descriptors promptly

The number of open descriptors should scale with:

```text
worker concurrency + bounded queue depth
```

not total tree size.

## 16.3 Memory behavior

Do not build a whole-tree plan in memory.

This project must handle trees containing millions of entries without memory scaling linearly with tree size.

Work must stream:

```text
discover -> compare -> enqueue -> execute
```

with backpressure.

---

# 17. Concurrency architecture

Do not use async I/O.

Use ordinary bounded OS threads.

Filesystem operations here are blocking and local; an async runtime buys little while dramatically increasing implementation surface.

The architecture has three logical roles:

```text
directory discovery
        |
        v
bounded work queue
        |
        v
file workers
        |
        v
atomic publication
```

Allow multiple directories to be scanned concurrently if benchmarks show this materially improves metadata-heavy trees.

Do not assume maximum parallelism is always fastest.

Large sequential files, HDDs and network-mounted filesystems can behave very differently from an Apple internal NVMe drive.

`-j 1` must always provide a fully valid deterministic-concurrency fallback.

## 17.1 Backpressure

The task queue must be bounded.

A good starting capacity is:

```text
jobs * 32
```

Benchmark.

The walker blocks when the queue is full.

This prevents:

* unbounded RAM
* unbounded open FDs
* millions of scheduled closures
* excessive work ahead of errors

## 17.2 Cancellation

Maintain a shared cancellation/error state.

On the first fatal error:

1. publish the error once
2. stop scheduling new mutations
3. workers abandon queued work as practical
4. current temp files clean themselves up through guards
5. join workers
6. exit nonzero

Do not continue mutating thousands of unrelated paths after a fatal correctness error.

---

# 18. Directory creation and finalization

New destination directories need special handling.

If the source directory has restrictive permissions, creating it immediately with those exact permissions may prevent the program from populating it.

For every newly created directory:

1. create with temporary owner-accessible permissions sufficient for population
2. keep that usable mode until all required copy and prune work is complete

Finalization is operation-specific:

* `cp`: finalize each corresponding directory post-order after every descendant
  copy/update completes.
* `sync`: Phase A creates and populates directories but applies no final
  directory xattrs, mode, or mtime. Phase B prunes destination-only entries,
  then finalizes each corresponding destination directory post-order while
  unwinding that same prune traversal.
* Existing destination directories retain their usable current mode until that
  finalization point.

Directory mtime is always applied last because both child creation and child
deletion invalidate it. Xattrs are propagated at finalization only when that
directory requires a mode, mtime, or other metadata mutation; merely visiting
a clean directory post-order is not an xattr scan. See the xattr policy below.

Implement a lightweight directory-completion mechanism.

A `cp` directory should finalize only when:

```text
scan finished
AND
all direct/indirect scheduled child mutations are complete
```

`sync` uses its required prune traversal as the corresponding post-order
finalization traversal; do not add a third whole-tree pass merely to apply
directory metadata.

A parent/child completion counter or scoped completion token is appropriate.

---

# 19. Metadata policy

V1 preserves:

### Regular files

* data
* POSIX permission mode
* modification time
* propagated extended attributes (not exact xattr equality)
* file type

### Directories

* POSIX permission mode
* modification time
* propagated extended attributes (not exact xattr equality)
* file type

### Symlinks

* link target

Do not promise preservation of:

* uid/gid
* atime
* birthtime
* POSIX ACLs
* macOS file flags
* Linux inode flags
* hardlink topology
* sparse extent topology
* Finder copy history
* filesystem compression state
* physical extent layout

Some optimized copy mechanisms may incidentally preserve more.

Those extras must not become contractual behavior unless explicitly added later.

## 19.1 Extended attributes

xattrs matter particularly on macOS and should not be dismissed as obscure metadata.

Use fd-based xattr APIs for regular files and directories.

rustix exposes `flistxattr`, `fgetxattr`, `fsetxattr`, and related operations on open descriptors.

V1 provides **xattr propagation, not xattr equality checking**:

1. when creating or atomically replacing an object, propagate every readable
   source xattr to the unpublished object before publication;
2. when an object is otherwise mutated or a directory is otherwise finalized,
   propagate source xattrs as part of that mutation where practical;
3. do not open or list an otherwise unchanged object solely to compare xattrs;
4. do not remove destination-only xattrs from an otherwise unchanged object.

Therefore xattr-only drift is intentionally not detected in the default mode,
and destination-only xattrs may remain. There is no xattr-verification flag in
V1. A future explicit full-metadata mode may add exact xattr convergence, but
it must not compromise the default `readdir/stat/compare` no-op path.

If an xattr cannot be read or written, treat it as an error rather than silently claiming convergence.

Symlink xattrs are explicitly outside V1 unless they can be implemented with equally strong fd-relative/no-follow guarantees on both platforms.

Do not fall back to reconstructing global paths merely to preserve symlink metadata.

---

# 20. Symlink replacement

To update a symlink:

1. read source link using parent FD + source name
2. compare exact raw link target bytes
3. if identical, skip
4. create a temporary sibling symlink in destination directory
5. revalidate destination stamp
6. atomically rename temporary symlink over destination symlink

Do not unlink the old link first.

That would create an unnecessary missing-name window.

---

# 21. `cp` execution algorithm

The high-level `cp` operation should be:

```text
validate roots
detect overlap
establish root device IDs

walk source tree
    for each source entry:
        inspect source safely
        inspect corresponding destination without following links

        destination absent:
            create/copy source object

        same type:
            converge object

        conflicting type:
            fail

wait for scheduled work
finalize directory metadata
return success
```

There is no second destination traversal.

Destination-only entries remain untouched.

---

# 22. `sync` execution algorithm

`sync` must be deliberately two-phase.

## Phase A: non-destructive convergence

Perform exactly the same overlay operation as `cp`.

Do **not** delete destination-only entries while source copying is still in progress.

If any source traversal/copy error occurs:

```text
abort
leave destination-only entries alone
```

This gives `sync` a valuable safety property:

> Failure while reading/copying source data cannot simultaneously erase unrelated destination data.

## Phase B: prune

Only after Phase A has fully succeeded:

1. capture the source root directory stamp at the end of Phase A;
2. revalidate that same source root stamp immediately before prune begins;
3. abort without beginning prune if it changed;
4. walk destination, compare each destination entry against source, remove
   destination-only entries, and finalize corresponding directories post-order.

```text
source root stable -> prune and finalize
```

This does require another destination-side traversal.

That cost is justified by much cleaner safety semantics.

Do not compromise this structure merely to avoid an extra directory walk.

---

# 23. Safe pruning

Deletion is the most dangerous part of this project.

Treat it accordingly.

## 23.1 Destination-only regular file/symlink

Immediately before unlink:

1. verify source counterpart is still absent
2. verify destination still matches expected stamp
3. unlink relative to destination parent FD

If either changed, fail or conservatively skip with a concurrent-change error.

Never unlink using a reconstructed absolute path.

For every corresponding source directory during prune:

1. open it by FD;
2. capture a directory `FileStamp` before examining membership;
3. process the corresponding destination directory;
4. re-stat the same source directory FD before finalizing the destination
   directory;
5. abort if `dev`, `ino`, type, `mtime`, or `ctime` changed.

This detects ordinary concurrent source-directory membership and identity
changes without pretending to provide snapshot isolation.

## 23.2 Destination-only directory

Do not recursively descend into a directory that remains addressable under an untrusted mutable pathname while another process could rename it.

Prefer:

1. verify source counterpart absent
2. verify destination identity
3. atomically rename the destination-only directory to an absent private
   temporary sibling name using a no-replace rename primitive
4. open the renamed directory safely
5. recursively delete through that FD
6. `statat(..., NOFOLLOW)` the private sibling name, verify the recorded
   directory identity, then remove the now-empty directory

If the final identity check fails, leave the name behind and report a cleanup
conflict. The renamed-aside directory uses the same short-name and
identity-checked cleanup contract as every temporary object.

The private rename itself must not overwrite an existing name. Use the
platform no-replace primitive (`renameat2(..., RENAME_NOREPLACE)` on Linux and
the corresponding Darwin primitive), retrying a new counter value on
collision. If the platform/filesystem cannot perform a no-replace rename, fail
that prune operation safely; never fall back to a plain overwriting `renameat`.

This establishes ownership of the namespace entry before destructive recursion.

If another process creates a new object at the original name afterward, the prune operation does not touch it.

Never recursively traverse a distinct mount instance during deletion unless
`--cross-file-systems` was explicitly supplied.

A destination-only mount instance should fail safely before descent.

---

# 24. Mount boundary enforcement

Capture a mount identity for each selected root and compare every child entry
against the corresponding root identity before operating on it. This is a
mount-instance policy, not merely a device-boundary policy.

On Linux:

```text
use statx(..., STATX_MNT_ID) where available
use openat2(..., RESOLVE_NO_XDEV) for FD-relative child resolution
```

[`STATX_MNT_ID`](https://man7.org/linux/man-pages/man2/statx.2.html) identifies
the mount containing an entry. [`RESOLVE_NO_XDEV`](https://man7.org/linux/man-pages/man2/openat2.2.html)
rejects traversal across mount points, including bind mounts. Use both where
the kernel supports them: mount IDs make the policy observable for every entry
and `openat2` enforces it while resolving the child. If neither facility is
available, default-mode Linux traversal must fail closed rather than claim
that `st_dev` proves bind-mount safety.

On macOS, use the strongest FD-relative volume/mount identity exposed by the
platform, using `st_dev` together with filesystem identity where appropriate.

Conceptually, unless:

```text
child.mount_identity == corresponding_root.mount_identity
```

the operation fails, unless:

```bash
--cross-file-systems
```

When a boundary is encountered without permission to cross:

```text
error: mount boundary at <path>; use --cross-file-systems to traverse
```

Do not merely skip it silently because that could make `sync` appear successful while leaving divergent data.

`--cross-file-systems` explicitly disables this restriction.

---

# 25. Durability

Default mode provides **atomic publication**, not expensive power-loss durability.

That is:

```text
write/clone temp
rename temp -> final
```

A normal observer never sees a half-written final file.

`--durable` requests stronger storage-ordering guarantees.

For a regular-file replacement:

```text
finish temp data + metadata
sync temp
rename temp -> final
sync parent directory
```

On Linux, use appropriate `fsync`/`fdatasync` semantics.

On macOS, investigate/use `F_FULLFSYNC` for the strongest available flush path rather than pretending ordinary `fsync` guarantees physical-media persistence. Apple's documentation explicitly distinguishes `fsync` from `F_FULLFSYNC`; the latter asks the drive to flush buffered data to permanent storage.

This mode is allowed to be dramatically slower.

Do not impose its cost on normal operation.

---

# 26. Progress UI

The progress display should be small, polished and dependency-free.

Think `pv`, not `npm`.

## 26.1 Output destination

Interactive progress goes to:

```text
stderr
```

Never stdout.

Detection:

```text
rustix::termios::isatty(stderr)
```

Width:

```text
rustix::termios::tcgetwinsize(stderr)
```

Both are exposed directly by rustix.

If stderr is not a terminal:

```text
no progress output
```

unless a future explicit force option is added.

`--no-progress` disables it.

## 26.2 Avoid flicker

Do not immediately render a progress bar for operations that finish in 12 ms.

Start rendering only after roughly:

```text
100–200 ms
```

of runtime.

Tune empirically.

## 26.3 No pre-scan for totals

This is critical.

Do **not** scan the source tree once merely to calculate a progress denominator and then scan it again to do the work.

Useful work must begin immediately.

While discovery remains active, show an indeterminate meter:

```text
⠹  184k scanned  12.8 GiB copied  3.4 GiB/s  931 changed
```

As the walker discovers changed files, atomically accumulate:

```text
planned_logical_bytes
planned_files
```

Workers accumulate:

```text
completed_logical_bytes
completed_files
physical_stream_bytes
hash_bytes
```

Once source discovery is complete, the denominator is frozen.

Then transform the same display into a determinate bar:

```text
12.8/18.2 GiB [██████████████░░░░░░] 70%  3.4 GiB/s  ETA 2s
```

The percentage must never move backwards.

Therefore do not display a percentage until planning for that phase is complete.

## 26.4 Refresh rate

Cap redraws around:

```text
10–20 Hz
```

There is no value in repainting the terminal on every copied block.

Use one lightweight progress renderer only when interactive progress is enabled.

Workers update atomics.

Do not send a progress event through a channel for every buffer read.

## 26.5 Terminal mechanics

Use only a single line.

Needed control sequences:

```text
\r
erase-to-end-of-line
```

No alternate screen.

No cursor hiding.

No complex terminal state.

No termios mode changes are necessary.

On completion:

1. draw final state
2. print newline

On error:

1. clear/terminate progress line
2. print diagnostic

## 26.6 Width adaptation

Use `tcgetwinsize`.

Prioritize fields approximately:

Wide terminal:

```text
18.2/52.7 GiB [████████████░░░░░░] 35%  3.41 GiB/s  18,442 files  ETA 10s
```

Medium:

```text
18.2/52.7 GiB [████████░░░░] 35%  3.41 GiB/s  ETA 10s
```

Narrow:

```text
18.2/52.7 GiB 35% 3.41 GiB/s
```

Very narrow:

```text
35% 3.41 GiB/s
```

Do not wrap.

## 26.7 Clone/reflink progress

A reflinked 20 GiB file may complete in milliseconds.

Count its **logical size** toward progress.

Effective throughput may therefore legitimately appear enormous.

This is fine; it reflects effective convergence throughput rather than physical disk-write bandwidth.

Optionally maintain separate internal counters for:

```text
logical_bytes
streamed_bytes
cloned_bytes
hashed_bytes
```

for summaries/benchmark instrumentation.

Do not clutter the normal progress line with all four.

## 26.8 Sync prune phase

After copy convergence:

```text
copy [complete]
```

then transition to:

```text
⠋ pruning  182k scanned  4,211 deleted
```

Do not pre-scan the destination purely to obtain a deletion count.

---

# 27. Progress statistics structure

Use atomics rather than locks for hot counters.

Conceptually:

```rust
struct Progress {
    scanned_entries: AtomicU64,
    compared_files: AtomicU64,

    planned_files: AtomicU64,
    planned_bytes: AtomicU64,

    completed_files: AtomicU64,
    completed_bytes: AtomicU64,

    streamed_bytes: AtomicU64,
    cloned_bytes: AtomicU64,
    hashed_bytes: AtomicU64,

    skipped_files: AtomicU64,
    deleted_entries: AtomicU64,

    discovery_done: AtomicBool,
    prune_active: AtomicBool,
    finished: AtomicBool,
}
```

The renderer samples these counters.

Do not let progress reporting contend materially with copy workers.

---

# 28. Path handling

Unix filenames are byte strings, not guaranteed UTF-8.

Correctness must work for arbitrary valid Unix names.

Never require:

```rust
path.to_str()
```

for filesystem operation.

Use `OsStr`/`CStr`/platform byte representations.

Only user display may be lossy or escaped.

For `--verbose` and error messages, implement a small escaping formatter so filenames containing:

* newline
* carriage return
* tabs
* control bytes
* backslash

cannot corrupt log structure.

Do not normalize Unicode filenames.

macOS's filesystem normalization behavior is outside this tool's semantic layer.

---

# 29. Error model

Exit codes:

```text
0 success
1 operational/correctness failure
2 invalid invocation
```

Do not reproduce rsync's large exit-code taxonomy.

Errors should contain:

```text
operation
relative path
OS error
```

Example:

```text
fs: copy "foo/bar": permission denied
```

or:

```text
fs: "foo/bar" changed while being copied
```

Prefer relative paths in diagnostics once roots are established.

---

# 30. Failure and interruption semantics

`cp` and `sync` are resumable through idempotence.

If interrupted:

* already atomically published files remain valid
* unpublished temporary files are not visible as final destination names
* existing destination files must never be left partially overwritten
* re-running the command continues convergence

For `sync`:

* prune never starts unless the entire source convergence phase succeeded
* prune itself may be partially complete if interrupted
* rerunning completes it

Do not claim transactionality for an entire tree.

The transaction boundary is an individual namespace mutation/file publication.

---

# 31. Concurrency race policy

The tool does not provide snapshot isolation.

A tree that is actively changing may cause the operation to fail. Detected
concurrent modification aborts further destructive work; it never rolls back
namespace mutations that completed atomically before detection.

That is preferable to silently producing an arbitrary mixture of versions.

Policy:

```text
source entry disappears after enumeration -> concurrent-change error
source object changes during copy -> concurrent-change error
destination object changes before replace -> concurrent-change error
type changes during operation -> concurrent-change error
```

For `sync` prune, source counterpart absence is revalidated immediately before
each destination-only deletion and corresponding source directories are
stamped before and after membership processing. A source directory can still
change after it has been checked and processed. Without filesystem snapshots,
no algorithm can make the entire tree appear frozen. Re-running against a
quiescent source converges the tree.

Do not add elaborate retry loops that can livelock on an actively changing tree.

At most, retry a narrow lookup race once or twice when doing so is obviously safe.

Then fail.

---

# 32. Performance philosophy

The hot path for a completed no-op run should approximately be:

```text
readdir
stat source
stat destination
compare tiny metadata structs
next
```

No:

* file opens when avoidable
* xattr opens/listing for otherwise unchanged objects
* hashing, except the defined unknown-timestamp-resolution correctness fallback
* allocation-heavy PathBuf construction
* global locks
* logging
* progress messages per entry
* persistent indexes

For a changed file on APFS:

```text
open source
clone to destination temp
metadata validation
rename
```

For a changed file on reflink-capable Linux:

```text
open source
create temp
FICLONE
metadata validation
rename
```

This is the core performance thesis.

---

# 33. Allocation discipline

The agent should inspect allocations in the hot traversal loop.

Avoid constructing:

```text
root.join(child).join(grandchild)...
```

for filesystem operations.

Child lookup should operate directly on the directory entry name.

Human-readable relative paths can be constructed lazily:

* when verbose output is enabled
* when progress wants to show a current path
* when an error occurs

Do not allocate a display string for every unchanged file merely because diagnostics might theoretically need it.

Reuse scratch buffers:

* copy buffer per worker
* xattr name/value buffers
* path-escaping buffer
* hash buffer

---

# 34. Reflink capability caching

Maintain an invocation-local capability cache.

Example key:

```text
(src_dev, dst_dev)
```

Possible state:

```text
Unknown
Supported
Unsupported
```

If cloning fails with an error clearly meaning the filesystem pair cannot clone:

```text
Unknown -> Unsupported
```

Future files skip the doomed clone attempt.

If one clone succeeds:

```text
Unknown -> Supported
```

Individual clone failures must still fall back appropriately.

Do not persist this information after process exit.

---

# 35. Sparse files

Do not promise sparse extent preservation in V1.

Reflink/clone/copy_file_range may preserve efficient extent behavior incidentally.

Buffered fallback may materialize holes.

Document this.

Only implement explicit `SEEK_DATA`/`SEEK_HOLE` sparse preservation later if benchmarks/use cases justify the complexity.

---

# 36. Hard links

Do not preserve hardlink topology in V1.

Two source paths referring to one inode may become two independent destination files.

If a particular source/destination counterpart is already literally the same inode, it can be treated as unchanged.

Do not build a source inode graph or hardlink index.

That would work against the project's streaming/stateless simplicity.

---

# 37. macOS-specific priorities

macOS is the optimization target.

Test at minimum:

* current macOS
* APFS internal SSD
* APFS external volume if available
* exFAT destination if available

Prioritize:

1. fd-relative traversal correctness
2. APFS cloning
3. xattrs
4. Apple Silicon performance
5. `F_FULLFSYNC` durable mode
6. terminal UX

Apple's file-copy APIs explicitly distinguish data, POSIX metadata and extended attributes, and offer fd-based copying.

---

# 38. Linux-specific priorities

Test at minimum where available:

* ext4
* btrfs
* XFS
* tmpfs

Prioritize:

1. fd-relative traversal
2. `openat2` hardening where available
3. `FICLONE`
4. `copy_file_range`
5. xattrs
6. mount-boundary handling

Correctness must still work on a filesystem supporting none of the fancy copy primitives.

The fallback path is always ordinary open/read/write/rename.

---

# 39. Tests: semantic matrix

Build integration tests for every combination of:

```text
source:
    absent
    file
    dir
    symlink

destination:
    absent
    file
    dir
    symlink
```

Verify the documented result or documented error.

Explicitly test:

```text
src
src/
dst
dst/
```

produce identical semantics.

Test both `cp` and `sync`.

Add root-establishment cases:

* an absent final destination leaf is valid when its parent exists;
* an absent intermediate destination component is an error and creates nothing;
* an intermediate source or destination symlink is an error;
* a final source symlink is copied as a symlink;
* an absent destination nested under the source directory is rejected before
  mutation.

---

# 40. Tests: idempotence

Every successful mutation test must immediately rerun the same command.

The second run must:

```text
exit 0
perform zero content copies
perform zero deletes
perform zero metadata mutations
```

Instrument internally in tests if necessary.

This is a fundamental invariant.

---

# 41. Tests: atomic replacement

For large files:

1. destination contains known old pattern
2. source contains known new pattern
3. copy while another thread repeatedly opens/reads destination
4. observed destination must always be either entirely old or entirely new

Never partially new.

Also kill the process during a large buffered copy.

After termination, final destination name must remain either:

```text
old complete file
```

or, if publication happened:

```text
new complete file
```

Never partial.

---

# 42. Tests: symlink attacks

These tests are mandatory.

Create:

```text
src-root/
dst-root/
outside-src/
outside-dst/
```

Put sentinel files outside both roots.

Run adversarial threads that repeatedly:

* replace directories with symlinks
* rename child directories
* swap symlinks and regular files
* change link targets
* replace destination entries during copy
* rename a temporary file/directory and replace its temporary name with a
  sentinel object before cleanup

The operation may:

```text
succeed
or
fail with concurrent-change error
```

It must **never**:

* read sentinel data through traversal
* modify outside destination root
* delete outside destination root
* unlink or rmdir a replacement at a temporary name after identity validation

Repeat race tests thousands of times.

These are more valuable than broad superficial unit-test coverage.

---

# 43. Tests: source mutation

While copying:

* truncate source
* append source
* rewrite same-size contents
* rename source path
* replace source pathname with symlink

Expected:

* opened FD prevents pathname substitution
* detectable content/metadata mutation causes failure
* no partial destination publication

---

# 44. Tests: destination mutation

While copying:

* replace destination inode
* modify destination contents
* rename destination elsewhere
* create destination after planner observed absence

Expected:

```text
conflict/error
```

Do not overwrite the unexpected object.

---

# 45. Tests: arbitrary filenames

Create entries containing:

```text
spaces
tabs
newlines
leading dash
backslashes
non-UTF-8 bytes
very long names
Unicode
```

All filesystem semantics must work.

Include a `NAME_MAX`-length final filename and verify that copy, replacement,
and private-prune naming still succeed without deriving a temporary name from
that filename.

Output formatting must remain legible/unambiguous.

---

# 46. Tests: metadata

Verify:

* file mode
* directory mode
* file mtime
* directory mtime after recursive completion
* `sync` directory mtime/mode finalization after prune, rather than after
  Phase A
* regular-file and directory xattr propagation on creation/replacement and
  applicable metadata mutation
* xattr-only destination drift remains unchanged in default mode, with no
  xattr enumeration or metadata mutation solely to detect it
* timestamp normalization on known coarse-resolution destination filesystems:
  a second run performs no data or metadata mutation
* unknown timestamp resolution: equal-size files with differing raw mtimes are
  BLAKE3-compared; equal content does not trigger recopy or repeated mtime-only
  mutation

Tests should feature capability detection and skip only when the test filesystem genuinely lacks xattr support.

---

# 47. Tests: sync safety

Critical scenarios:

### Empty source

```text
src/
dst/
  lots
  of
  data
```

`cp` leaves destination entries.

`sync` removes them.

### Copy phase failure

Cause a permission/read error halfway through source convergence.

Verify `sync` has **not begun destination-only prune**.

### Mounted child

Where CI/platform permissions allow:

```text
dst/mounted-volume
```

must not be recursively pruned without:

```text
--cross-file-systems
```

On Linux, additionally create a bind mount and verify that default mode fails
before operating on it, including for a mounted non-directory where the test
environment permits one. `--cross-file-systems` is the only opt-out.

### Source-directory mutation during prune

Mutate a source directory after Phase A and before Phase B; verify prune does
not start. Also mutate a corresponding source directory while it is being
pruned; verify detection aborts further destructive work, preserves the
already-completed individual mutations without rollback, and reports a
concurrent-change error.

---

# 48. Tests: dry-run

For every major mutation scenario:

1. snapshot destination
2. run `--dry-run`
3. byte/metadata compare destination against snapshot
4. confirm no temp files/directories were created
5. verify reported operations match what a real run subsequently does

---

# 49. Tests: hash mode

Use same-size/same-mtime files with deliberately different contents.

Default metadata mode should exhibit its documented fast-path behavior.

`--check=hash` must detect the mismatch.

Also test:

* identical contents, different mtime
* BLAKE3 says equal → metadata-only convergence
* zero-byte files
* multi-gigabyte sparse/test files where practical

---

# 50. Randomized model tests

Do not necessarily add `proptest`.

Build a small deterministic test generator using an internal simple PRNG.

Generate random trees containing:

```text
dirs
files
symlinks
destination extras
metadata differences
content differences
```

Maintain a simple in-memory semantic model.

For hundreds/thousands of generated trees:

```text
actual cp result == model cp result
actual sync result == model sync result
```

Then rerun and verify idempotence.

Use a fixed seed when reporting failure.

---

# 51. Performance benchmarks

Do not use Criterion in the shipped dependency graph.

A benchmark script using the release binary is sufficient.

Optionally use external `hyperfine` when installed.

Benchmark against:

* macOS `/bin/cp` where semantically comparable
* system `rsync`
* `rclone` only as contextual comparison
* Linux `cp`
* Linux `rsync`

Do not contort semantics merely to win a benchmark.

## Benchmark A: startup

Tiny no-op invocation.

Goal:

> The binary should feel instantaneous.

Measure process startup separately from filesystem workload.

## Benchmark B: no-op tree

Generate:

```text
100k files
1M files where practical
mostly small
deep + wide layouts
source and destination already converged
```

This is one of the primary benchmarks.

On a known-resolution filesystem with no metadata differences, the tool must
perform no content reads, hashing, or xattr opens/listing in metadata mode.

Profile syscall counts.

## Benchmark C: small-file initial copy

Examples:

```text
100k × 4 KiB
100k × 32 KiB
```

Measure:

* wall time
* CPU
* peak RSS
* files/sec

Experiment with `-j`.

## Benchmark D: large files

```text
1 GiB
10 GiB
multiple large files
```

Measure:

* APFS clone path
* Linux reflink path
* cross-filesystem streamed path

Confirm reflink/clone is actually selected rather than merely assuming it.

## Benchmark E: mixed real-world source tree

Use something analogous to:

```text
large Rust checkout
node_modules-like tree
build output
photo/media directory
```

## Benchmark F: hash mode

Test:

```text
cached large files
uncached large files
many medium files
```

On Apple Silicon specifically benchmark:

```text
single-thread BLAKE3
file-level parallel BLAKE3
update_rayon thresholds
```

Do not assume more parallelism is faster when storage rather than CPU is limiting.

## Benchmark G: progress overhead

Run identical workloads:

```text
progress on
progress off
```

Progress rendering should produce effectively negligible throughput impact.

---

# 52. Performance acceptance criteria

Avoid brittle hardware-specific absolute numbers.

Instead enforce principles:

### No-op

* zero file-content reads in metadata mode
* zero hashing when timestamp resolution is known and metadata is unambiguous
* zero destination writes
* bounded memory
* metadata traversal should be competitive with or faster than rsync on local trees

### APFS same-volume changed file

* clone fast path actually used when supported
* runtime should be primarily metadata/namespace cost rather than proportional to file bytes

### Linux reflink filesystem

Same expectation with `FICLONE`.

### Streamed large-file copy

* should approach the practical throughput of platform `cp`
* worker abstraction must not introduce a large penalty

### Memory

For million-entry trees:

```text
memory must not scale with total entry count
```

Only bounded queues, worker scratch space and live directory contexts should dominate.

### CPU

Metadata-mode no-op runs should not burn CPU hashing or formatting paths.

---

# 53. Profiling requirement

Before calling performance work complete, profile on macOS with appropriate platform tools and on Linux with `perf`/equivalent.

Look for:

* unnecessary `stat` duplication
* PathBuf allocation
* lock contention
* excessive FD duplication
* failed reflink syscall repeated per file
* progress rendering overhead
* buffer allocation
* excessive syscalls per unchanged entry

Performance changes must be justified by profiles or benchmarks, not intuition alone.

---

# 54. Release profile

Use an optimized release configuration such as:

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "abort"
strip = true
incremental = false
```

Do not globally hardcode:

```text
target-cpu=native
```

because distributed binaries need portability across supported machines.

Local benchmarking may use it separately.

---

# 55. Unsafe policy

Keep unsafe code isolated.

Most filesystem logic should use rustix's safe wrappers.

Expected unsafe code should be concentrated in the narrow platform modules:

```text
platform/macos.rs
platform/linux.rs
```

for Darwin or Linux interfaces not wrapped by rustix, such as FD-based
timestamp-resolution and mount-identity queries. Keep all raw syscall details
out of traversal and mutation logic.

Add:

```rust
#![deny(unsafe_op_in_unsafe_fn)]
```

Every unsafe block must have a short `SAFETY:` comment stating its invariant.

Do not spread raw libc calls throughout traversal logic.

---

# 56. Explicit non-goals

V1 must not implement:

* remote hosts
* SSH
* network protocol
* rsync wire compatibility
* daemon mode
* config files
* include/exclude patterns
* glob syntax
* `.gitignore`
* compression
* bandwidth limiting
* resume databases
* persistent hashes
* persistent indexes
* content-addressed storage
* snapshots
* versioning
* rollback
* multiple source arguments
* directory basename insertion
* trailing-slash semantics
* symlink following
* type-conflict replacement
* ACL preservation
* ownership preservation
* hardlink graph preservation
* exact sparse topology preservation
* device files
* sockets
* FIFOs
* Windows
* TUI
* JSON output
* plugins

If an implementation task begins drifting into one of these, stop.

The project's strength is the things it refuses to become.

---

# 57. Implementation order

Do not implement everything simultaneously.

## Phase 1 — semantics/core

Implement:

* CLI parser
* FD-relative root establishment with no-follow intermediate components
* final-destination-parent existence rule
* root type semantics
* trailing slash invariance
* fd-relative directory traversal
* file/dir/symlink support
* type conflicts
* metadata comparator
* simple buffered atomic file replacement
* `cp`
* basic tests

No concurrency yet.

Get invariants right first.

## Phase 2 — sync safety

Implement:

* two-phase sync
* fd-relative prune
* rename-to-private-directory deletion
* mount boundaries
* mount-instance identity / Linux bind-mount enforcement
* overlap detection, including an absent destination leaf
* source/destination identity revalidation
* source-directory stability checks during prune
* identity-checked temporary cleanup
* adversarial symlink/race tests

Do not optimize before this is solid.

## Phase 3 — metadata

Implement:

* mode
* mtime
* destination timestamp-resolution normalization and unknown-resolution hash fallback
* directory finalization
* xattr propagation without xattr equality scans
* metadata idempotence tests

## Phase 4 — platform copy acceleration

macOS first:

* APFS clone fast path
* fallback copy implementation
* capability caching

Linux:

* `FICLONE`
* `copy_file_range`
* buffered fallback

Benchmark each before proceeding.

## Phase 5 — bounded concurrency

Implement:

* bounded worker queue
* reusable per-worker buffers
* cancellation
* `-j`
* directory completion accounting

Verify all race tests again under high concurrency.

## Phase 6 — BLAKE3

Implement:

```text
--check=hash
```

Then benchmark Apple Silicon thresholds for:

* single-thread hashing
* file-level parallelism
* BLAKE3 Rayon updates

Optimize only based on results.

## Phase 7 — progress

Implement:

* tty detection
* width detection
* delayed rendering
* indeterminate scan mode
* determinate copy mode after discovery
* throughput EMA
* ETA
* prune mode
* no-progress flag

No terminal UI dependency.

## Phase 8 — durability

Implement:

* `--durable`
* Linux sync sequence
* macOS full-sync sequence
* parent-directory sync
* documentation

## Phase 9 — performance hardening

Run the complete benchmark suite.

Profile.

Remove:

* allocations
* redundant stats
* needless opens
* synchronization bottlenecks

Only then choose final auto-job count and buffer sizes.

---

# 58. Code quality requirements

Prefer straightforward state machines over clever generic abstractions.

A reviewer should be able to answer:

```text
What object does this FD refer to?
Can this lookup follow a symlink?
Can this mutation escape the destination root?
What happens if the pathname changes right now?
What happens if the process dies right now?
Does rerunning repair this state?
```

by reading a small amount of nearby code.

Avoid generic filesystem traits unless they genuinely make tests or platform implementations clearer.

This is systems software.

Make invariants visible.

---

# 59. Required internal documentation

At the top of the filesystem engine, document these invariants explicitly:

```text
1. Supplied roots are resolved FD-by-FD with no-follow intermediate components;
   only the final destination leaf may be absent.
2. Recursive traversal uses directory FDs and single-component names.
3. Symlinks are never followed; a final source symlink is copied as an object.
4. Type changes are never performed implicitly.
5. Final regular-file publication is atomic via same-directory rename.
6. Destination identity is revalidated before destructive replacement.
7. Sync deletion begins only after successful copy convergence and source-root
   revalidation; it is not a snapshot transaction.
8. Recursive deletion first takes control of destination-only directories and
   cleans their private names only after identity verification.
9. Default traversal rejects mount-instance crossings unless explicitly opted
   out with --cross-file-systems.
10. Xattrs are propagated on mutation, not equality-scanned on no-op objects.
11. Queues and open FDs are bounded.
12. No persistent state is written.
```

Tests should map directly to these invariants.

---

# 60. Definition of done

The project is ready for an initial release only when all of the following hold:

### Semantics

* trailing slash never changes behavior
* exactly one SRC/DST
* directories always denote tree roots
* source/destination existence does not alter grammar
* no implicit intermediate destination creation
* symlinks never traversed
* type conflicts fail

### Correctness

* repeated invocation is a no-op
* changed files are atomically published
* copy failures never expose partial final files
* sync prune waits for successful convergence
* timestamp normalization does not cause repeated copies on coarse filesystems
* arbitrary Unix filenames work

### Safety

* source/destination overlap detected
* path races do not escape root FDs
* adversarial symlink tests cannot touch sentinels outside roots
* destination races cannot silently clobber unexpected objects
* mount instances, including Linux bind mounts, are not crossed by default
* temporary cleanup never removes an identity it cannot establish as owned

### Performance

* no persistent index/cache
* no pre-scan
* no hashing in the normal known-resolution default no-op path
* no xattr scans for otherwise unchanged default-mode objects
* APFS clone works
* Linux reflink works where supported
* copy queues bounded
* buffers reused
* million-entry scan memory bounded
* progress overhead negligible

### UX

* normal CLI fits on one help screen
* no trailing-slash footnotes
* no compatibility option zoo
* default output clean
* progress bar appears only for meaningful-duration interactive operations
* progress resembles `pv`: compact, useful, no theatrics

### Maintainability

* unsafe isolated
* platform specialization isolated
* no unnecessary dependency growth
* no rsync protocol concepts
* invariants prominently documented

---

# 61. Final design principle

When deciding whether to add behavior, use this test:

> Can a user predict the result from `SRC`, `DST`, and the verb without remembering historical Unix syntax?

If not, do not add it.

When deciding whether to add an optimization, use this test:

> Does it avoid work while preserving exactly the same observable semantics and safety invariants?

If yes, pursue it aggressively.

This tool should ultimately feel almost boring:

```bash
fs cp src dst
fs sync src dst
```

but internally use the best filesystem mechanisms available to make those two commands **safe to repeat, difficult to misuse, and extremely fast**.
