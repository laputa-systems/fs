# Repository guide

`fs` is a stateless filesystem-convergence CLI for macOS and Linux.

Build: `make build`; test: `make test`; Clippy check: `cargo clippy --all-targets --all-features -- --deny warnings`.

## Invariants

- Establish roots component by component from held directory descriptors; reject intermediate symlinks. See [docs/safety.md](docs/safety.md).
- Perform recursive lookups and mutations relative to held directory descriptors and one name component. See [docs/safety.md](docs/safety.md).
- Revalidate planned identities before mutations and publish complete regular files with a same-directory atomic rename. See [docs/safety.md](docs/safety.md).
- Begin `sync` pruning only after copy convergence and source-root revalidation; completed mutations are not rolled back. See [docs/safety.md](docs/safety.md).
- Keep traversal within the current mount by default; crossing mounts requires `--cross-file-systems`. See [docs/safety.md](docs/safety.md).
- Bound file queues, worker buffers, and live directory descriptors; spill wide directory work during real runs. See [docs/architecture.md](docs/architecture.md).
- Propagate xattrs on mutations; do not enumerate them to prove a no-op. See [docs/metadata.md](docs/metadata.md).
- Keep persistent indexes, caches, manifests, and daemon state out of the filesystem. See [docs/architecture.md](docs/architecture.md).
- Isolate unsafe operations in platform wrappers and explain each unsafe block locally with `SAFETY:`. See [docs/safety.md](docs/safety.md).

## Topic map

- User installation, commands, options, and output: [README.md](README.md).
- Engine and platform structure: [docs/architecture.md](docs/architecture.md).
- Path, mutation, mount, and durability guarantees: [docs/safety.md](docs/safety.md).
- Comparison and metadata propagation: [docs/metadata.md](docs/metadata.md).
- Test commands and coverage: [docs/testing.md](docs/testing.md).
- Release targets and Linux build workflow: [docs/release.md](docs/release.md).
- Incomplete validation and performance work: [TODO.md](TODO.md).
