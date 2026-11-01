use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Wasm parsing error: {0}")]
    WasmParse(#[from] wasmparser::BinaryReaderError),

    #[error("invalid WebAssembly module: {0}")]
    InvalidModule(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("DWARF error: {0}")]
    Dwarf(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Transport error: {0}")]
    Transport(#[from] wasm_dap_protocol::transport::TransportError),

    #[error("{}", format_runtime_error(.0))]
    Runtime(#[from] wasmtime::Error),

    #[error("Invalid configuration: {0}")]
    Config(String),

    #[error("{0}")]
    Session(String),

    #[error("evaluation failed: {0}")]
    Eval(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<gimli::Error> for Error {
    fn from(e: gimli::Error) -> Self {
        Error::Dwarf(e.to_string())
    }
}

/// Formats a Wasmtime error with its full cause chain on one line.
pub fn format_runtime_error(error: &wasmtime::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in error.chain() {
        let text = cause.to_string();
        let first = text.lines().next().unwrap_or_default().trim().to_string();
        let is_backtrace_header = first.starts_with("error while executing at wasm backtrace");
        if !first.is_empty() && !is_backtrace_header && !parts.contains(&first) {
            parts.push(first);
        }
    }
    if parts.is_empty() {
        error.to_string()
    } else {
        parts.join(": ")
    }
}
