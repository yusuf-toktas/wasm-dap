//! Shared test support for the wasm-dap crates.

pub mod dap;
pub mod fixtures;

pub use dap::{DapClient, find_var};
pub use fixtures::{
    RustTarget, line_of, rust_fixture, rust_fixture_opt, rust_fixture_with, source_map, wat_module,
    wat_source,
};
