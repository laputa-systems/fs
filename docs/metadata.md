# Comparison and metadata

The comparison and propagation policy is implemented in
[`src/engine/compare.rs`](../src/engine/compare.rs) and
[`src/metadata.rs`](../src/metadata.rs).

For regular files, metadata mode compares size, permission bits, and the
modification time normalized to the destination filesystem's timestamp grid.
When timestamp precision is unknown, same-size files with differing raw times
receive a BLAKE3 content comparison instead of being recopied based only on an
ambiguous timestamp. Hash mode always compares BLAKE3 content. Equal content
can still receive mode and timestamp updates.

Creation, replacement, and metadata updates propagate the applicable source
mode, modification time, and readable extended attributes. The no-op path does
not enumerate xattrs, so it does not prove exact xattr equality. Directory
metadata is finalized after recursive work; `sync` finalizes it after pruning.

V1 does not preserve ownership, ACLs, hard-link topology, or sparse extents.
