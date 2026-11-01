use wasmtime::{ExternType, Global, Linker, Memory, Ref, SharedMemory, Store, Table, Tag, Val};

use super::Program;
use super::host::HostState;
use crate::error::{Error, Result};

/// Prefix of the error raised when the guest calls an import nobody provides.
pub const UNRESOLVED_IMPORT: &str = "call to unresolved import";

/// Builds a linker that provides WASI preview 1 and placeholder definitions
/// for every other import, so any core module can be instantiated.
///
/// Placeholder functions trap with a descriptive error when called; memories,
/// tables and globals are created with their declared minimum size and zero
/// values.
pub fn build_linker(program: &Program, store: &mut Store<HostState>) -> Result<Linker<HostState>> {
    let mut linker: Linker<HostState> = Linker::new(&program.engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut HostState| &mut state.wasi)?;

    for import in program.module.imports() {
        let module = import.module().to_string();
        let name = import.name().to_string();
        if linker.get(&mut *store, &module, &name).is_ok() {
            continue;
        }
        match import.ty() {
            ExternType::Func(ty) => {
                let message = format!("{UNRESOLVED_IMPORT} `{module}::{name}`");
                linker.func_new(&module, &name, ty, move |_, _, _| {
                    Err(wasmtime::Error::msg(message.clone()))
                })?;
            }
            ExternType::Memory(ty) => {
                if ty.is_shared() {
                    let memory = SharedMemory::new(&program.engine, ty)?;
                    linker.define(&mut *store, &module, &name, memory)?;
                } else {
                    let memory = Memory::new(&mut *store, ty)?;
                    linker.define(&mut *store, &module, &name, memory)?;
                }
            }
            ExternType::Global(ty) => {
                let value = Val::default_for_ty(ty.content()).ok_or_else(|| {
                    Error::Unsupported(format!(
                        "cannot synthesize a value for imported global `{module}::{name}`"
                    ))
                })?;
                let global = Global::new(&mut *store, ty, value)?;
                linker.define(&mut *store, &module, &name, global)?;
            }
            ExternType::Table(ty) => {
                let init = Ref::null(ty.element().heap_type());
                let table = Table::new(&mut *store, ty, init)?;
                linker.define(&mut *store, &module, &name, table)?;
            }
            ExternType::Tag(ty) => {
                let tag = Tag::new(&mut *store, &ty)?;
                linker.define(&mut *store, &module, &name, tag)?;
            }
        }
    }
    Ok(linker)
}
