//! Stack frames of a paused debuggee and access to their state.

use wasmtime::{AsContextMut, FrameHandle, Instance, StoreContextMut, Val};

use super::FrameInfo;
use super::program::ProgramInfo;
use super::values::Place;
use crate::dwarf::{
    DebugInfo, ExprContext, FrameVariables, Location, Variable, VariableLocation, WasmStorage, expr,
};
use crate::runtime::{HostState, val_to_bytes};

const MAX_FRAMES: usize = 10_000;

pub(crate) struct FrameData {
    pub handle: FrameHandle,
    pub pc: Option<u32>,
}

/// Collects all WebAssembly frames, innermost first.
pub(crate) fn collect(store: &mut StoreContextMut<'_, HostState>) -> Vec<FrameData> {
    let exits: Vec<FrameHandle> = store.debug_exit_frames().collect();
    let mut frames = Vec::new();
    for exit in exits {
        let mut current = Some(exit);
        while let Some(frame) = current {
            if frames.len() >= MAX_FRAMES {
                return frames;
            }
            let pc = frame
                .wasm_function_index_and_pc(store.as_context_mut())
                .ok()
                .flatten()
                .map(|(_, pc)| pc.raw());
            current = frame.parent(store.as_context_mut()).ok().flatten();
            frames.push(FrameData { handle: frame, pc });
        }
    }
    frames
}

pub(crate) fn describe(info: &ProgramInfo, index: usize, frame: &FrameData) -> FrameInfo {
    let id = index as i64 + 1;
    match frame.pc {
        Some(pc) => FrameInfo {
            id,
            name: info
                .function_name_at(pc)
                .unwrap_or_else(|| "<unknown>".to_string()),
            pc: Some(pc),
            function_index: info.module.function_at(pc).map(|f| f.index),
            source: info.code_source(pc),
        },
        None => FrameInfo {
            id,
            name: "<host>".to_string(),
            pc: None,
            function_index: None,
            source: None,
        },
    }
}

pub(crate) fn local(
    store: &mut StoreContextMut<'_, HostState>,
    frame: &FrameHandle,
    index: u32,
) -> Option<Val> {
    let count = frame.num_locals(store.as_context_mut()).ok()?;
    if index >= count {
        return None;
    }
    frame.local(store.as_context_mut(), index).ok()
}

pub(crate) fn stack(
    store: &mut StoreContextMut<'_, HostState>,
    frame: &FrameHandle,
    index: u32,
) -> Option<Val> {
    let count = frame.num_stacks(store.as_context_mut()).ok()?;
    if index >= count {
        return None;
    }
    frame.stack(store.as_context_mut(), index).ok()
}

pub(crate) fn global(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Instance,
    index: u32,
) -> Option<Val> {
    let global = instance.debug_global(store.as_context_mut(), index)?;
    Some(global.get(store.as_context_mut()))
}

/// Machine state view used to evaluate DWARF location expressions in a frame.
pub(crate) struct FrameAccess<'s, 'a> {
    pub store: &'s mut StoreContextMut<'a, HostState>,
    pub frame: Option<&'s FrameHandle>,
    pub instance: Option<Instance>,
    pub frame_base: Option<u64>,
}

impl ExprContext for FrameAccess<'_, '_> {
    fn wasm_value(&mut self, storage: WasmStorage, index: u32) -> Option<Vec<u8>> {
        let value = match storage {
            WasmStorage::Local => local(self.store, self.frame?, index)?,
            WasmStorage::Stack => stack(self.store, self.frame?, index)?,
            WasmStorage::Global => global(self.store, self.instance?, index)?,
        };
        val_to_bytes(&value)
    }

    fn frame_base(&mut self) -> Option<u64> {
        self.frame_base
    }

    fn read_memory(&mut self, address: u64, size: usize) -> Option<Vec<u8>> {
        super::memory::read_exact(self.store, self.instance?, address, size).ok()
    }
}

/// Instance that owns a frame, falling back to the program instance.
pub(crate) fn frame_instance(
    store: &mut StoreContextMut<'_, HostState>,
    frame: Option<&FrameHandle>,
    fallback: Option<Instance>,
) -> Option<Instance> {
    frame
        .and_then(|f| f.instance(store.as_context_mut()).ok())
        .or(fallback)
}

/// Resolves where each DWARF variable of a frame lives.
pub(crate) fn locate_variables(
    store: &mut StoreContextMut<'_, HostState>,
    debug: &DebugInfo,
    frame: &FrameData,
    instance: Option<Instance>,
    scope: &FrameVariables,
) -> Vec<(Variable, Place)> {
    let Some(pc) = frame.pc else {
        return Vec::new();
    };
    let frame_base = scope.frame_base.as_ref().and_then(|expr| {
        let mut access = FrameAccess {
            store,
            frame: Some(&frame.handle),
            instance,
            frame_base: None,
        };
        expr::evaluate_frame_base(expr, scope.encoding, &mut access).ok()
    });
    scope
        .variables
        .iter()
        .map(|variable| {
            let mut access = FrameAccess {
                store,
                frame: Some(&frame.handle),
                instance,
                frame_base,
            };
            let size = variable.ty.and_then(|t| debug.type_size(t));
            let place = locate(variable, pc, scope.encoding, size, &mut access);
            (variable.clone(), place)
        })
        .collect()
}

pub(crate) fn locate(
    variable: &Variable,
    pc: u32,
    encoding: gimli::Encoding,
    size: Option<u64>,
    access: &mut FrameAccess<'_, '_>,
) -> Place {
    match &variable.location {
        VariableLocation::Const(bytes) => Place::Bytes(bytes.clone()),
        VariableLocation::Missing => Place::Unavailable("optimized out".into()),
        VariableLocation::Expr(_) | VariableLocation::List(_) => {
            let Some(expression) = variable.expression_at(pc) else {
                return Place::Unavailable("not available here".into());
            };
            match expr::evaluate(expression, encoding, access) {
                Ok(location) => location_to_place(location, size, access),
                Err(e) => Place::Unavailable(e.to_string()),
            }
        }
    }
}

fn location_to_place(
    location: Location,
    size: Option<u64>,
    access: &mut FrameAccess<'_, '_>,
) -> Place {
    match location {
        Location::Memory(address) => Place::Memory(address),
        Location::Value(bytes) => Place::Bytes(bytes),
        Location::Wasm(storage, index) => match access.wasm_value(storage, index) {
            // A value too small to hold the variable holds its address, which is
            // how Zig describes parameters passed by reference.
            Some(bytes) if bytes.len() <= 8 && size.is_some_and(|s| s > bytes.len() as u64) => {
                let mut buf = [0u8; 8];
                buf[..bytes.len()].copy_from_slice(&bytes);
                Place::Memory(u64::from_le_bytes(buf))
            }
            Some(bytes) => Place::Bytes(bytes),
            None => Place::Unavailable("WebAssembly value unavailable".into()),
        },
        Location::Pieces(pieces) => {
            let mut bytes = Vec::new();
            for (piece, size) in pieces {
                let size = size as usize;
                let chunk = match piece {
                    Location::Memory(address) => access.read_memory(address, size),
                    Location::Value(b) => Some(b),
                    Location::Wasm(storage, index) => access.wasm_value(storage, index),
                    _ => None,
                };
                let Some(mut chunk) = chunk else {
                    return Place::Unavailable("optimized out".into());
                };
                chunk.resize(size, 0);
                bytes.extend(chunk);
            }
            Place::Bytes(bytes)
        }
        Location::Unavailable => Place::Unavailable("optimized out".into()),
    }
}
