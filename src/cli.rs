//! Command-line parsing and user-facing invocation setup.

use std::ffi::OsString;
use std::num::NonZeroUsize;

use lexopt::Arg;

use crate::error::{FsError, Result};

/// The only operations exposed by the command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Cp,
    Sync,
}

/// Equality proof requested by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckMode {
    Metadata,
    Hash,
}

/// Parsed options.  Filesystem safety policy is intentionally not exposed as
/// additional CLI knobs; these are the complete V1 options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Options {
    pub(crate) dry_run: bool,
    pub(crate) verbose: bool,
    pub(crate) jobs: Option<NonZeroUsize>,
    pub(crate) check: CheckMode,
    pub(crate) durable: bool,
    pub(crate) cross_file_systems: bool,
    pub(crate) no_progress: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            verbose: false,
            jobs: None,
            check: CheckMode::Metadata,
            durable: false,
            cross_file_systems: false,
            no_progress: false,
        }
    }
}

/// A complete, syntactically valid invocation before roots are established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandLine {
    pub(crate) operation: Operation,
    pub(crate) options: Options,
    pub(crate) source: OsString,
    pub(crate) destination: OsString,
}

/// Non-error outcomes handled by the process entrypoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParseOutcome {
    Run,
    Help,
    Version,
}

/// Parse process arguments, retaining path arguments as [`OsString`] so
/// arbitrary Unix names (including non-UTF-8 names) remain valid.
///
/// The returned outcome is `Help` or `Version` when those options were
/// requested; in those cases no positional arguments are required.
pub(crate) fn parse_args() -> Result<(ParseOutcome, Option<CommandLine>)> {
    parse_args_from(std::env::args_os())
}

/// Testable form of [`parse_args`] accepting any iterator whose first item is
/// the executable name.
pub(crate) fn parse_args_from<I>(args: I) -> Result<(ParseOutcome, Option<CommandLine>)>
where
    I: IntoIterator<Item = OsString>,
{
    let mut parser = lexopt::Parser::from_iter(args);
    let mut options = Options::default();
    let mut positional = Vec::with_capacity(3);
    let mut outcome = ParseOutcome::Run;

    while let Some(arg) = parser.next()? {
        match arg {
            Arg::Short('n') => options.dry_run = true,
            Arg::Short('v') => options.verbose = true,
            Arg::Short('h') => outcome = ParseOutcome::Help,
            Arg::Short('V') => outcome = ParseOutcome::Version,
            Arg::Short('j') => {
                let value = parser
                    .value()?
                    .into_string()
                    .map_err(|value| FsError::usage(format!("invalid jobs value {:?}", value)))?;
                options.jobs = Some(parse_jobs(&value)?);
            }
            Arg::Long("dry-run") => options.dry_run = true,
            Arg::Long("verbose") => options.verbose = true,
            Arg::Long("durable") => options.durable = true,
            Arg::Long("cross-file-systems") => options.cross_file_systems = true,
            Arg::Long("no-progress") => options.no_progress = true,
            Arg::Long("help") => outcome = ParseOutcome::Help,
            Arg::Long("version") => outcome = ParseOutcome::Version,
            Arg::Long("jobs") => {
                let value = parser
                    .value()?
                    .into_string()
                    .map_err(|value| FsError::usage(format!("invalid jobs value {:?}", value)))?;
                options.jobs = Some(parse_jobs(&value)?);
            }
            Arg::Long("check") => {
                let value = parser.value()?.into_string().map_err(|value| {
                    FsError::usage(format!("invalid --check value {:?}", value))
                })?;
                options.check = parse_check(&value)?;
            }
            Arg::Long(name) if name.starts_with("check=") => {
                options.check = parse_check(&name[6..])?;
            }
            Arg::Value(value) => positional.push(value),
            other => return Err(FsError::usage(other.unexpected().to_string())),
        }
    }

    if outcome != ParseOutcome::Run {
        return Ok((outcome, None));
    }
    if positional.len() != 3 {
        return Err(FsError::usage(
            "expected exactly one operation and two paths: fs cp [OPTIONS] SRC DST",
        ));
    }

    let operation = match positional.remove(0).as_os_str() {
        value if value == "cp" => Operation::Cp,
        value if value == "sync" => Operation::Sync,
        _ => {
            return Err(FsError::usage("operation must be exactly `cp` or `sync`"));
        }
    };

    Ok((
        ParseOutcome::Run,
        Some(CommandLine {
            operation,
            options,
            source: positional.remove(0),
            destination: positional.remove(0),
        }),
    ))
}

fn parse_jobs(value: &str) -> Result<NonZeroUsize> {
    let jobs = value
        .parse::<NonZeroUsize>()
        .map_err(|_| FsError::usage("--jobs requires a positive integer"))?;
    Ok(jobs)
}

fn parse_check(value: &str) -> Result<CheckMode> {
    match value {
        "metadata" => Ok(CheckMode::Metadata),
        "hash" => Ok(CheckMode::Hash),
        _ => Err(FsError::usage("--check must be `metadata` or `hash`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<I, S>(args: I) -> Result<(ParseOutcome, Option<CommandLine>)>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        parse_args_from(args.into_iter().map(Into::into))
    }

    #[test]
    fn parses_tiny_command_line_without_utf8_paths() {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt;
        #[cfg(unix)]
        let source = OsString::from_vec(b"src-\xff".to_vec());
        #[cfg(not(unix))]
        let source = OsString::from("src");
        let parsed = parse([
            OsString::from("fs"),
            OsString::from("cp"),
            OsString::from("--check=hash"),
            OsString::from("-j"),
            OsString::from("2"),
            source.clone(),
            OsString::from("dst"),
        ])
        .unwrap()
        .1
        .unwrap();
        assert_eq!(parsed.operation, Operation::Cp);
        assert_eq!(parsed.source, source);
        assert_eq!(parsed.options.check, CheckMode::Hash);
        assert_eq!(parsed.options.jobs.unwrap().get(), 2);
    }

    #[test]
    fn rejects_extra_source_and_zero_jobs() {
        assert!(parse(["fs", "cp", "a", "b", "c", "d"]).is_err());
        assert!(parse(["fs", "cp", "-j", "0", "a", "b"]).is_err());
    }
}
