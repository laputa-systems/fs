# Remaining work

The core `cp`/`sync` implementation and its safety tests are in place. This file
tracks work that is not yet complete or not yet evidenced; it is deliberately
not a feature wishlist.

## Performance evidence

- Run the full release benchmark set from `scripts/bench.sh` and record results
  for startup, metadata no-op, initial small-file copy, large-file copy, mixed
  trees, hash mode, and progress overhead.
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
- Run the bounded-FD and million-entry-scale checks under representative Linux
  and macOS resource limits, then retain the measured results.
- Document the benchmark and platform-validation evidence alongside release
  artifacts.

## Explicitly deferred, not TODO

Sparse extent preservation, hard-link topology, ACL/ownership preservation,
remote transfer, persistent indexes, resume databases, configuration files,
include/exclude rules, JSON/TUI output, and Windows support remain intentional
non-goals. They should not be implemented merely to empty this file.
