//! wasm-dap core: a WebAssembly debugging engine built on Wasmtime's guest
//! debugging API, DWARF source mapping, module analysis, execution tracing
//! and a Debug Adapter Protocol server.

pub mod debugger;
pub mod dwarf;
pub mod error;
pub mod evaluator;
pub mod launch;
pub mod module;
pub mod runtime;
pub mod server;
pub mod trace;

pub use error::{Error, Result};
