//! Thin binary shim: delegate the whole invocation to [`gateway_server::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    gateway_server::run(std::env::args_os().skip(1))
}
