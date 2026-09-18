//! `nixit` binary entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    nixit::cli::run()
}
