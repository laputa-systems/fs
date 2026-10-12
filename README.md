# fs

A small, safe, fast filesystem convergence tool for macOS and Linux.

```sh
fs cp [OPTIONS] SRC DST     # overlay SRC onto DST, keeping destination-only entries
fs sync [OPTIONS] SRC DST   # converge DST to SRC, pruning destination-only entries
```

Options: `-n/--dry-run`, `-v/--verbose`, `-j/--jobs N`, `--check=metadata|hash`,
`--durable`, `--cross-file-systems`, `--no-progress`, `-h/--help`, `-V/--version`.

Directory traversal is descriptor-relative and never follows symlinks, and files
are published by atomic rename. The full contract is in [`AGENTS.md`](AGENTS.md);
unfinished work is tracked in [`TODO.md`](TODO.md).

## Development

```sh
make build                  # debug build
make test                   # cargo test
make lint                   # clippy --fix
make release                # static release build (build-std, immediate-abort panics)
make install                # release build copied to ~/usr/bin
```

`make release-dynamic`, `make verify-release`, and `make verify-release-dynamic`
are used by the Linux release image (`Dockerfile`) and the release workflow.
`TARGET` selects the target triple; it defaults to the host, with Linux hosts
mapped to musl.
