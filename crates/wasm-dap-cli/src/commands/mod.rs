pub mod disasm;
pub mod info;
pub mod serve;
pub mod trace;

use std::path::Path;

use anyhow::{Context, Result};
use wasm_dap_core::dwarf::DebugInfo;
use wasm_dap_core::module::WasmModule;
use wasm_dap_core::runtime::read_module;

/// Reads and parses a module without compiling it.
pub fn load_static(path: &Path) -> Result<(WasmModule, Option<DebugInfo>)> {
    let source = read_module(path).with_context(|| format!("failed to read {}", path.display()))?;
    let module = WasmModule::parse(source.bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let debug = DebugInfo::load_with_bias(module.bytes(), source.dwarf_bias)
        .ok()
        .flatten();
    Ok((module, debug))
}
