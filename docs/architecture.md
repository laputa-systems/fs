# Architecture

`fs` is a stateless Unix CLI. It keeps no index or other persistent
application state.

The command parser in [`src/cli.rs`](../src/cli.rs) creates typed operations
and options. [`src/path.rs`](../src/path.rs) establishes source and destination
roots. [`src/engine/sync.rs`](../src/engine/sync.rs) coordinates overlay,
convergence, and pruning; traversal, copying, and deletion are handled by
[`src/engine/traverse.rs`](../src/engine/traverse.rs),
[`src/engine/copy.rs`](../src/engine/copy.rs), and
[`src/engine/delete.rs`](../src/engine/delete.rs).

The platform layer in [`src/platform/`](../src/platform/) holds narrow OS
operations. macOS attempts an FD-relative clone and then buffered copying.
Linux tries reflink, `copy_file_range`, and then a reusable buffered loop.
Unsupported clone pairs are remembered only for the current invocation.

Regular-file work uses the bounded queue in
[`src/workers.rs`](../src/workers.rs), with reusable per-worker buffers.
During real runs, directory work spills from memory to an anonymous spool
when it grows, and completed directory scopes release their descriptors.
Dry runs stream child directories without retaining wide-directory records or
creating temporary objects. They keep streams open for at most 32 ancestor
levels; deeper directories are rescanned to select each next name. This keeps
memory and descriptor use bounded, with quadratic read work for unusually deep
wide directories.

[`src/progress.rs`](../src/progress.rs) renders a delayed terminal meter on
stderr. It is disabled by `--no-progress` and when stderr is not a terminal.
