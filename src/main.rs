//! Binary entry point: parse the command line, run the viewer, report errors.
//!
//! The terminal is set up and torn down inside [`app::run`] so that a failure to
//! connect is printed on the normal screen, with the shell untouched.

use std::process::ExitCode;

use sqlv::app::{self, Invocation, USAGE};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match app::Options::parse(args) {
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Invocation::Version) => {
            println!("sqlv {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Invocation::Run(options)) => match app::run(options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("sqlv: {error}");
                if let Some(hint) = error.hint() {
                    eprintln!("hint: {hint}");
                }
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("sqlv: {message}");
            eprintln!();
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
