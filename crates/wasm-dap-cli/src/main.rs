mod cli;
#[macro_use]
mod output;
mod commands;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    let result = match cli.command {
        Command::Info(args) => commands::info::run(args),
        Command::Disasm(args) => commands::disasm::run(args),
        Command::Trace(args) => commands::trace::run(args),
        Command::Serve(args) => commands::serve::run(args),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Diagnostics always go to stderr: stdout carries the DAP stream in `serve`.
fn init_tracing(filter: &str) {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}
