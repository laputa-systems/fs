# Remaining work

The core `cp`/`sync` implementation and its safety tests are in place. This file
tracks work that is not yet complete or not yet evidenced; it is deliberately
not a feature wishlist.

## Performance evidence

- Extend `scripts/bench.sh` to measure startup, metadata no-op, small and large
  copies, mixed trees, hash mode, and progress overhead, then record release
  results. It currently measures population, metadata and hash no-op runs, and
  a one-file change in a generated flat tree.
- Profile representative macOS and Linux runs for duplicate stats, unnecessary
  opens, FD duplication, queue contention, failed clone attempts, allocations,
  and progress-rendering overhead.
- Use those measurements to validate the current buffer size, worker heuristic,
  clone/copy thresholds, and bounded-memory behavior rather than treating the
  current constants as final.

## Filesystem validation

- Validate Linux behavior on ext4 and tmpfs, including the fallback path when
  clone and in-kernel copy acceleration are unavailable.
- Validate macOS behavior on the supported APFS configurations and durable
  publication paths.
- Exercise coarse and unknown timestamp-resolution filesystems where available.

## Stress and release readiness

- Expand the adversarial symlink, source-mutation, and destination-mutation
  suites into repeated stress runs suitable for release qualification.
- Bound dry-run buffering for very wide directories without creating a
  temporary filesystem object.
- Repeat the existing bounded-FD checks and add a million-entry-scale check
  under representative Linux and macOS resource limits; retain the results.
- Document the benchmark and platform-validation evidence alongside release
  artifacts.

## Explicitly deferred, not TODO

Sparse and hard-link preservation, ACL/ownership, remote transfer, persistent
state, configuration and include/exclude rules, JSON/TUI output, and Windows
support remain deferred. They are not active tasks.
