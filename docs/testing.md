# Testing

## Commands

- `make build` builds the debug binary.
- `make test` runs the test suite for the host (Linux defaults to the musl target).
- `cargo clippy --all-targets --all-features -- --deny warnings` runs Clippy without applying fixes.
- `make test-ci TARGET=<target>` runs the test suite in release mode for the selected target.

The `make lint` target invokes Clippy with `--fix --allow-dirty` and can modify
source files. Use the direct Clippy command above for a read-only check.

## Coverage

Unit tests live beside their implementation. Integration tests cover command
semantics and modeled trees in [`tests/semantics.rs`](../tests/semantics.rs)
and [`tests/model.rs`](../tests/model.rs), timestamp behavior in
[`tests/metadata.rs`](../tests/metadata.rs), race and symlink handling in
[`tests/symlink_races.rs`](../tests/symlink_races.rs), bounded concurrency and
descriptor use in [`tests/concurrency.rs`](../tests/concurrency.rs), and
interruption behavior in [`tests/interruption.rs`](../tests/interruption.rs).

The release workflow runs `make test-ci` for each target, using the Dockerfile
builder for Linux and the host for macOS.

To run the Linux test suite locally, build the image from
[`Dockerfile`](../Dockerfile) and invoke the test target inside it:

```sh
docker build -t fs-builder .
docker run --rm -v "$PWD:/fs" -w /fs fs-builder make test-ci
```
