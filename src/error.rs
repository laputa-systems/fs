//! Error types shared across command-line and filesystem operations.
//!
//! The command line deliberately has only two failure classes: an invalid
//! invocation (exit status 2) and an operational/correctness failure (exit
//! status 1).  Keeping that distinction here means callers do not have to
//! inspect error strings to choose an exit status.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;

/// Result type used by the command-line and root-establishment boundary.
pub(crate) type Result<T> = std::result::Result<T, FsError>;

/// User-facing failure with the exit status required by the contract.
#[derive(Debug)]
pub(crate) enum FsError {
    /// The command line is malformed or has an unsupported combination of
    /// arguments.  These errors use exit status 2.
    Usage(String),

    /// An operation failed while interacting with a path.  These errors use
    /// exit status 1.
    Io {
        operation: String,
        path: OsString,
        source: io::Error,
    },

    /// A path violates an invocation-time safety rule, such as an absent
    /// intermediate component or a NUL byte.  These errors use exit status 2.
    InvalidPath {
        operation: String,
        path: OsString,
        reason: String,
    },

    /// A discovered filesystem state cannot be handled safely.  These errors
    /// use exit status 1.
    Conflict {
        operation: String,
        path: OsString,
        reason: String,
    },
}

impl FsError {
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    pub(crate) fn invalid_path(
        operation: impl Into<String>,
        path: &OsStr,
        reason: impl Into<String>,
    ) -> Self {
        Self::InvalidPath {
            operation: operation.into(),
            path: path.to_os_string(),
            reason: reason.into(),
        }
    }

    pub(crate) fn conflict(
        operation: impl Into<String>,
        path: &OsStr,
        reason: impl Into<String>,
    ) -> Self {
        Self::Conflict {
            operation: operation.into(),
            path: path.to_os_string(),
            reason: reason.into(),
        }
    }

    pub(crate) fn io(
        operation: impl Into<String>,
        path: &OsStr,
        source: impl Into<io::Error>,
    ) -> Self {
        Self::Io {
            operation: operation.into(),
            path: path.to_os_string(),
            source: source.into(),
        }
    }

    /// Return the process exit status prescribed by the CLI contract.
    pub(crate) const fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::InvalidPath { .. } => 2,
            Self::Io { .. } | Self::Conflict { .. } => 1,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "fs: {message}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "fs: {operation} \"{}\": {source}", escape_os(path)),
            Self::InvalidPath {
                operation,
                path,
                reason,
            }
            | Self::Conflict {
                operation,
                path,
                reason,
            } => write!(f, "fs: {operation} \"{}\": {reason}", escape_os(path)),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Escape a Unix filename for diagnostics without requiring it to be UTF-8.
///
/// The result is intentionally a compact, log-safe representation.  It is
/// only for display; the original [`OsString`] is retained for every lookup.
pub(crate) fn escape_os(value: &OsStr) -> String {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

    #[cfg(unix)]
    let bytes = value.as_bytes();
    #[cfg(not(unix))]
    let bytes = value.to_string_lossy().as_bytes();

    let mut output = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(byte as char),
            _ => output.push_str(&format!("\\x{byte:02x}")),
        }
    }
    output
}

impl From<lexopt::Error> for FsError {
    fn from(error: lexopt::Error) -> Self {
        Self::usage(error.to_string())
    }
}
