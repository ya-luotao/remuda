use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use remuda::cli::{self, Cli, Context};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let ctx = Context {
        env: std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
        cwd: std::env::current_dir().ok(),
        now: jiff::Timestamp::now(),
        clock: jiff::Timestamp::now,
        tz: jiff::tz::TimeZone::system(),
        stdin_is_tty: std::io::stdin().is_terminal(),
        stdout_is_tty: std::io::stdout().is_terminal(),
        stderr_is_tty: std::io::stderr().is_terminal(),
    };
    cli::run(cli, &ctx)
}
