use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "wasm-dap",
    version,
    about = "WebAssembly debugger, execution tracer and module inspector",
    long_about = "WebAssembly debugger, execution tracer and module inspector.\n\n\
                  `wasm-dap serve` runs a Debug Adapter Protocol server for editors such as \
                  Visual Studio Code. The other commands work directly on the command line."
)]
pub struct Cli {
    /// Log filter for diagnostics written to stderr (e.g. `warn`, `debug`).
    #[arg(long, global = true, env = "WASM_DAP_LOG", default_value = "warn")]
    pub log: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the structure of a module: imports, exports, functions, memories and debug info.
    Info(InfoArgs),
    /// Disassemble function bodies, annotated with DWARF source lines.
    Disasm(DisasmArgs),
    /// Run a program and trace function calls, returns and traps.
    Trace(TraceArgs),
    /// Run the Debug Adapter Protocol server.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Module to inspect (`.wasm` or `.wat`).
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DisasmArgs {
    /// Module to disassemble (`.wasm` or `.wat`).
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Only disassemble functions whose name or index (`func[N]`) matches.
    #[arg(long, short = 'f', value_name = "NAME")]
    pub function: Option<String>,

    /// Do not annotate instructions with source lines.
    #[arg(long)]
    pub no_source: bool,
}

#[derive(Debug, Args)]
pub struct TraceArgs {
    /// Program to run (`.wasm` or `.wat`).
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Program arguments, or the function arguments when `--invoke` is used.
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<String>,

    /// Call this exported function instead of the WASI entry point `_start`.
    #[arg(long, value_name = "FUNCTION")]
    pub invoke: Option<String>,

    /// Also record every executed instruction (slow).
    #[arg(long)]
    pub instructions: bool,

    /// Stop recording after this many events; the program still runs to completion.
    #[arg(long, default_value_t = 10_000)]
    pub max_events: usize,

    /// Print the trace as JSON (program output is included as events).
    #[arg(long)]
    pub json: bool,

    /// Preopen a host directory for the guest, as `HOST[::GUEST]`.
    #[arg(long = "dir", value_name = "HOST[::GUEST]")]
    pub dirs: Vec<String>,

    /// Set an environment variable for the guest, as `KEY=VALUE`.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Map a DWARF path prefix to a local one, as `FROM=TO`.
    #[arg(long = "source-map", value_name = "FROM=TO")]
    pub source_map: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Listen on this TCP port on 127.0.0.1 instead of using stdio; 0 picks a free port.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Secret that TCP clients must send as `serverToken` in the launch
    /// configuration. A random token is generated and printed when omitted.
    /// Prefer the environment variable: other local users can read command lines.
    #[arg(long, env = "WASM_DAP_TOKEN", value_name = "TOKEN", requires = "port")]
    pub token: Option<String>,
}
