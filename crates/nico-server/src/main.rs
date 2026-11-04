//! Thin binary shim: delegate the whole invocation to [`nico_server::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    nico_server::run(std::env::args_os().skip(1))
}
