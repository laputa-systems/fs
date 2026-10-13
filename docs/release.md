# Release builds

The release workflow is manually dispatched from
[`.github/workflows/release.yml`](../.github/workflows/release.yml). It builds
these targets:

| Target | Archives |
| --- | --- |
| `aarch64-apple-darwin` | `fs-aarch64-apple-darwin.zip` |
| `x86_64-unknown-linux-musl` | `fs-x86_64-unknown-linux-musl.zip`, `fs-x86_64-unknown-linux-musl-static.zip` |
| `aarch64-unknown-linux-musl` | `fs-aarch64-unknown-linux-musl.zip`, `fs-aarch64-unknown-linux-musl-static.zip` |

The workflow creates a prerelease tagged `release-<commit-sha>`. Linux
artifacts are built in the Alpine-based builder defined by
[`Dockerfile`](../Dockerfile); it uses the pinned toolchain in
[`rust-toolchain.toml`](../rust-toolchain.toml) and the build modes in
[`Makefile`](../Makefile).

Build and verify a static Linux release with `make release TARGET=<target>`
and `make verify-release TARGET=<target>`. The dynamic variant uses
`make release-dynamic TARGET=<target>` and
`make verify-release-dynamic TARGET=<target>`. The workflow runs
both variants for Linux and builds the Apple Silicon macOS archive. Test
commands are in [`testing.md`](testing.md).
