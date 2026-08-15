//! Core copy, synchronization, traversal, comparison, and deletion behavior.
//!
//! # Filesystem invariants
//!
//! 1. [`crate::path`] establishes supplied roots component-by-component from
//!    held directory FDs without following intermediate links; only a final
//!    destination leaf may be absent.
//! 2. Recursive traversal uses directory FDs and a single child component per
//!    lookup.
//! 3. Symlinks are never followed; a final source symlink is copied as an
//!    object.
//! 4. Implicit type changes are rejected.
//! 5. Final regular-file publication is an atomic same-directory `renameat`.
//! 6. Planned destination identity is revalidated before destructive
//!    replacement.
//! 7. `sync` deletion starts only after successful copy convergence and
//!    source-root revalidation; it is not a snapshot transaction.
//! 8. Recursive deletion first takes destination-only directories private and
//!    cleans their private names only after identity verification.
//! 9. Default traversal rejects mount-instance crossings unless
//!    `--cross-file-systems` opts in.
//! 10. Xattrs propagate on mutation, never through an equality scan of an
//!     otherwise unchanged object.
//! 11. File queues are bounded and directory completion scopes release
//!     completed directory FDs promptly.
//! 12. This engine writes no persistent state.

pub(crate) mod compare;
pub(crate) mod copy;
pub(crate) mod delete;
pub(crate) mod sync;
pub(crate) mod traverse;
