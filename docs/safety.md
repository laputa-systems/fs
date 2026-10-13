# Filesystem safety

[`src/path.rs`](../src/path.rs) resolves roots one component at a time from
held directory descriptors. Intermediate components must be real directories;
only the final destination component may be absent. Recursive operations use
held directory descriptors and one name component at a time. Symlinks are
never followed.

Directory-entry type bits are hints. Before opening or mutating an entry, the
engine re-stats it without following links. It also revalidates the planned
source and destination identities before copying, metadata changes, replacement,
and temporary cleanup.

Regular files are built under a private name in the destination directory and
published with one atomic rename. A partial copy never appears at the final
name. Cleanup checks that a temporary name still identifies the object the
operation created. POSIX still permits a narrow replacement between that check
and unlink; cleanup never follows the replacement.

`sync` first completes copy convergence and revalidates the source root. Only
then does it prune destination-only entries. It is not a snapshot transaction:
an error does not roll back mutations already completed. Recursive deletion
moves destination-only directories under private names before removing their
contents, and verifies identities before cleanup.

Traversal stays on the current mount by default. Linux uses `openat2` with
`RESOLVE_NO_XDEV` where available and mount IDs from `statx` as a fallback.
`--cross-file-systems` disables this boundary.

Without `--durable`, publication is atomic but does not promise power-loss
durability. With it, temporary file data and metadata are synchronized before
rename, and parent directories are synchronized after namespace changes.
macOS uses its stronger full-sync operation for regular files where available.

Unsafe operations are confined to narrow platform wrappers. Each unsafe block
has a local `SAFETY:` explanation, and `#![deny(unsafe_op_in_unsafe_fn)]`
remains enabled.
