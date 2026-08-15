#![deny(unsafe_op_in_unsafe_fn)]

// The FD-relative safety model and platform copy primitives are deliberately
// Unix contracts. V1 does not offer a weaker path-based Windows fallback.
#[cfg(not(unix))]
compile_error!("fs supports Unix targets only");

mod cli;
mod error;
mod path;

mod engine;

mod hash;
mod metadata;
mod platform;
mod progress;
mod workers;

fn main() {
    match cli::parse_args() {
        Ok((cli::ParseOutcome::Help, _)) => print_help(),
        Ok((cli::ParseOutcome::Version, _)) => println!("fs {}", env!("CARGO_PKG_VERSION")),
        Ok((cli::ParseOutcome::Run, Some(command))) => {
            if let Err(error) = engine::sync::execute(command) {
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
        }
        Ok((cli::ParseOutcome::Run, None)) => {
            eprintln!("fs: internal error: run outcome had no command");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.exit_code());
        }
    }
}

fn print_help() {
    print!(
        "fs cp [OPTIONS] SRC DST\n\
         fs sync [OPTIONS] SRC DST\n\n\
         Options:\n\
           -n, --dry-run\n\
           -v, --verbose\n\
           -j, --jobs N\n\
               --check=metadata|hash\n\
               --durable\n\
               --cross-file-systems\n\
               --no-progress\n\
           -h, --help\n\
           -V, --version\n"
    );
}
