//! Scopes and variables of a paused debuggee.

use wasmtime::{AsContextMut, Instance, Mutability, StoreContextMut, Val};

use super::frames::{self, FrameData};
use super::memory::format_memory_reference;
use super::program::ProgramInfo;
use super::values::{Children, TypedValue, ValueReader};
use super::{ScopeInfo, VariableInfo};
use crate::error::{Error, Result};
use crate::runtime::HostState;
use crate::runtime::values::{format_val, parse_val_str, val_as_i64, val_type_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    Locals,
    WasmLocals,
    OperandStack,
    Globals,
    Memory,
}

pub(crate) enum Container {
    Scope {
        frame: usize,
        kind: ScopeKind,
    },
    Value {
        value: TypedValue,
        path: Option<String>,
    },
}

/// Variable references handed out during one pause.
#[derive(Default)]
pub(crate) struct VarTable {
    containers: Vec<Container>,
}

impl VarTable {
    pub fn add(&mut self, container: Container) -> i64 {
        self.containers.push(container);
        self.containers.len() as i64
    }

    fn get(&self, reference: i64) -> Option<&Container> {
        let index = usize::try_from(reference.checked_sub(1)?).ok()?;
        self.containers.get(index)
    }
}

/// Inspection state of one pause; dropped when execution resumes.
pub(crate) struct Paused {
    pub frames: Vec<FrameData>,
    pub vars: VarTable,
    pub instance: Option<Instance>,
}

impl Paused {
    pub fn frame_index(&self, frame_id: i64) -> Result<usize> {
        usize::try_from(frame_id - 1)
            .ok()
            .filter(|i| *i < self.frames.len())
            .ok_or_else(|| Error::Session(format!("unknown frame {frame_id}")))
    }

    pub fn frame_pcs(&self) -> Vec<u32> {
        self.frames.iter().filter_map(|f| f.pc).collect()
    }
}

pub(crate) fn scopes(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    frame_id: i64,
) -> Result<Vec<ScopeInfo>> {
    let index = paused.frame_index(frame_id)?;
    let frame = &paused.frames[index];
    let mut scopes = Vec::new();
    let has_dwarf_scope = frame
        .pc
        .zip(info.debug.as_ref())
        .and_then(|(pc, debug)| debug.frame_variables(pc))
        .is_some();
    if has_dwarf_scope {
        scopes.push(ScopeInfo {
            name: "Locals".into(),
            reference: paused.vars.add(Container::Scope {
                frame: index,
                kind: ScopeKind::Locals,
            }),
            hint: Some("locals"),
            named: None,
            indexed: None,
            expensive: false,
        });
    }
    let locals = frame.handle.num_locals(store.as_context_mut()).unwrap_or(0) as usize;
    let stack = frame.handle.num_stacks(store.as_context_mut()).unwrap_or(0) as usize;
    scopes.push(ScopeInfo {
        name: "Wasm Locals".into(),
        reference: paused.vars.add(Container::Scope {
            frame: index,
            kind: ScopeKind::WasmLocals,
        }),
        hint: Some(if has_dwarf_scope {
            "registers"
        } else {
            "locals"
        }),
        named: Some(locals),
        indexed: None,
        expensive: false,
    });
    scopes.push(ScopeInfo {
        name: "Operand Stack".into(),
        reference: paused.vars.add(Container::Scope {
            frame: index,
            kind: ScopeKind::OperandStack,
        }),
        hint: Some("registers"),
        named: None,
        indexed: Some(stack),
        expensive: false,
    });
    scopes.push(ScopeInfo {
        name: "Globals".into(),
        reference: paused.vars.add(Container::Scope {
            frame: index,
            kind: ScopeKind::Globals,
        }),
        hint: None,
        named: Some(info.module.globals.len()),
        indexed: None,
        expensive: false,
    });
    if !info.module.memories.is_empty() {
        scopes.push(ScopeInfo {
            name: "Memory".into(),
            reference: paused.vars.add(Container::Scope {
                frame: index,
                kind: ScopeKind::Memory,
            }),
            hint: None,
            named: Some(info.module.memories.len()),
            indexed: None,
            expensive: false,
        });
    }
    Ok(scopes)
}

pub(crate) fn variables(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    reference: i64,
    start: Option<usize>,
    count: Option<usize>,
    hex: bool,
) -> Result<Vec<VariableInfo>> {
    let (frame, kind, value, path) = match paused.vars.get(reference) {
        Some(Container::Scope { frame, kind }) => (Some(*frame), Some(*kind), None, None),
        Some(Container::Value { value, path }) => (None, None, Some(value.clone()), path.clone()),
        None => {
            return Err(Error::Session(format!(
                "unknown variables reference {reference}"
            )));
        }
    };
    if let Some(value) = value {
        return value_children(
            store,
            info,
            paused,
            &value,
            path.as_deref(),
            start,
            count,
            hex,
        );
    }
    let frame = frame.unwrap_or(0);
    let start = start.unwrap_or(0);
    let list = match kind.unwrap_or(ScopeKind::WasmLocals) {
        ScopeKind::Locals => dwarf_locals(store, info, paused, frame, hex),
        ScopeKind::WasmLocals => wasm_locals(store, info, paused, frame, hex),
        ScopeKind::OperandStack => operand_stack(store, info, paused, frame, hex),
        ScopeKind::Globals => globals(store, info, paused, frame, hex),
        ScopeKind::Memory => memories(store, info, paused, frame),
    };
    Ok(match count {
        Some(count) => list.into_iter().skip(start).take(count).collect(),
        None => list.into_iter().skip(start).collect(),
    })
}

fn frame_instance(
    store: &mut StoreContextMut<'_, HostState>,
    paused: &Paused,
    frame: usize,
) -> Option<Instance> {
    frames::frame_instance(
        store,
        paused.frames.get(frame).map(|f| &f.handle),
        paused.instance,
    )
}

/// Builds the variable entry for a typed value, registering its children.
#[allow(clippy::too_many_arguments)]
pub(crate) fn typed_entry(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    instance: Option<Instance>,
    name: String,
    value: TypedValue,
    path: Option<String>,
    hex: bool,
) -> VariableInfo {
    let Some(debug) = info.debug.as_ref() else {
        return VariableInfo {
            name,
            value: "<no debug information>".into(),
            ..VariableInfo::default()
        };
    };
    let formatted = ValueReader {
        store,
        instance,
        debug,
    }
    .format(&value, hex);
    let (reference, named, indexed) = match formatted.children {
        Children::None => (0, None, None),
        Children::Named(n) => (
            paused.vars.add(Container::Value {
                value,
                path: path.clone(),
            }),
            Some(n),
            None,
        ),
        Children::Indexed(n) => (
            paused.vars.add(Container::Value {
                value,
                path: path.clone(),
            }),
            None,
            Some(n),
        ),
    };
    VariableInfo {
        name,
        value: formatted.value,
        type_name: Some(formatted.type_name),
        reference,
        named,
        indexed,
        memory_reference: formatted.memory_reference,
        evaluate_name: path,
        read_only: false,
    }
}

fn dwarf_locals(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    frame: usize,
    hex: bool,
) -> Vec<VariableInfo> {
    let Some(debug) = info.debug.clone() else {
        return Vec::new();
    };
    let Some(pc) = paused.frames.get(frame).and_then(|f| f.pc) else {
        return Vec::new();
    };
    let Some(scope) = debug.frame_variables(pc) else {
        return Vec::new();
    };
    let instance = frame_instance(store, paused, frame);
    let located = frames::locate_variables(store, &debug, &paused.frames[frame], instance, &scope);
    located
        .into_iter()
        .map(|(variable, place)| {
            let path = Some(variable.name.clone());
            typed_entry(
                store,
                info,
                paused,
                instance,
                variable.name,
                TypedValue {
                    ty: variable.ty,
                    place,
                },
                path,
                hex,
            )
        })
        .collect()
}

fn pointer_reference(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Option<Instance>,
    value: &Val,
) -> Option<String> {
    let address = u64::try_from(val_as_i64(value)?).ok()?;
    let size = super::memory::size(store, instance?, 0)?;
    (address > 0 && address < size).then(|| format_memory_reference(0, address))
}

fn wasm_locals(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &Paused,
    frame: usize,
    hex: bool,
) -> Vec<VariableInfo> {
    let Some(data) = paused.frames.get(frame) else {
        return Vec::new();
    };
    let function = data
        .pc
        .and_then(|pc| info.module.function_at(pc))
        .map(|f| f.index);
    let params = function
        .and_then(|f| info.module.func_sig(f))
        .map(|s| s.params.len() as u32)
        .unwrap_or(0);
    let instance = frame_instance(store, paused, frame);
    let count = data.handle.num_locals(store.as_context_mut()).unwrap_or(0);
    (0..count)
        .filter_map(|i| {
            let value = frames::local(store, &data.handle, i)?;
            let name = function
                .and_then(|f| info.module.local_name(f, i))
                .map(|n| format!("${n}"))
                .unwrap_or_else(|| {
                    if i < params {
                        format!("$param{i}")
                    } else {
                        format!("$local{i}")
                    }
                });
            Some(VariableInfo {
                evaluate_name: Some(format!("$local{i}")),
                memory_reference: pointer_reference(store, instance, &value),
                name,
                value: format_val(&value, hex),
                type_name: Some(val_type_name(&value).to_string()),
                read_only: true,
                ..VariableInfo::default()
            })
        })
        .collect()
}

fn operand_stack(
    store: &mut StoreContextMut<'_, HostState>,
    _info: &ProgramInfo,
    paused: &Paused,
    frame: usize,
    hex: bool,
) -> Vec<VariableInfo> {
    let Some(data) = paused.frames.get(frame) else {
        return Vec::new();
    };
    let instance = frame_instance(store, paused, frame);
    let count = data.handle.num_stacks(store.as_context_mut()).unwrap_or(0);
    (0..count)
        .filter_map(|i| {
            let value = frames::stack(store, &data.handle, i)?;
            Some(VariableInfo {
                name: format!("[{i}]"),
                evaluate_name: Some(format!("$stack{i}")),
                memory_reference: pointer_reference(store, instance, &value),
                value: format_val(&value, hex),
                type_name: Some(val_type_name(&value).to_string()),
                read_only: true,
                ..VariableInfo::default()
            })
        })
        .collect()
}

pub(crate) fn global_display_name(info: &ProgramInfo, index: u32) -> String {
    info.module
        .global_name(index)
        .unwrap_or_else(|| format!("global{index}"))
}

fn globals(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &Paused,
    frame: usize,
    hex: bool,
) -> Vec<VariableInfo> {
    let Some(instance) = frame_instance(store, paused, frame) else {
        return Vec::new();
    };
    (0..info.module.globals.len() as u32)
        .filter_map(|i| {
            let value = frames::global(store, instance, i)?;
            let mutable = info.module.globals[i as usize].mutable;
            Some(VariableInfo {
                name: global_display_name(info, i),
                evaluate_name: Some(format!("$global{i}")),
                memory_reference: pointer_reference(store, Some(instance), &value),
                value: format_val(&value, hex),
                type_name: Some(val_type_name(&value).to_string()),
                read_only: !mutable,
                ..VariableInfo::default()
            })
        })
        .collect()
}

fn memories(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &Paused,
    frame: usize,
) -> Vec<VariableInfo> {
    let instance = frame_instance(store, paused, frame);
    (0..info.module.memories.len() as u32)
        .map(|i| {
            let page = info.module.memories[i as usize].page_size;
            let bytes = instance.and_then(|inst| super::memory::size(store, inst, i));
            let value = match bytes {
                Some(b) => format!("{} pages ({b} bytes)", b / page.max(1)),
                None => "unavailable".to_string(),
            };
            VariableInfo {
                name: info.module.memory_name(i),
                value,
                type_name: Some("memory".into()),
                memory_reference: Some(format_memory_reference(i, 0)),
                read_only: true,
                ..VariableInfo::default()
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn value_children(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    value: &TypedValue,
    path: Option<&str>,
    start: Option<usize>,
    count: Option<usize>,
    hex: bool,
) -> Result<Vec<VariableInfo>> {
    let debug = info
        .debug
        .clone()
        .ok_or_else(|| Error::Session("no debug information".into()))?;
    let instance = paused.instance.or_else(|| frame_instance(store, paused, 0));
    let children = ValueReader {
        store,
        instance,
        debug: &debug,
    }
    .children(value, start.unwrap_or(0), count)?;
    Ok(children
        .into_iter()
        .map(|child| {
            let child_path = path.map(|p| {
                if child.name == "*" {
                    format!("*{p}")
                } else {
                    format!("{p}{}", child.accessor)
                }
            });
            typed_entry(
                store,
                info,
                paused,
                instance,
                child.name,
                child.value,
                child_path,
                hex,
            )
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn set_variable(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    reference: i64,
    name: &str,
    text: &str,
    hex: bool,
) -> Result<VariableInfo> {
    enum Target {
        Scope(usize, ScopeKind),
        Value(TypedValue, Option<String>),
    }
    let target = match paused.vars.get(reference) {
        Some(Container::Scope { frame, kind }) => Target::Scope(*frame, *kind),
        Some(Container::Value { value, path }) => Target::Value(value.clone(), path.clone()),
        None => {
            return Err(Error::Session(format!(
                "unknown variables reference {reference}"
            )));
        }
    };
    match target {
        Target::Scope(frame, ScopeKind::Globals) => {
            set_global(store, info, paused, frame, name, text, hex)
        }
        Target::Scope(_, ScopeKind::WasmLocals | ScopeKind::OperandStack) => Err(Error::Session(
            "WebAssembly locals and operand stack values are read-only: Wasmtime's \
                 debugging API does not support modifying them"
                .into(),
        )),
        Target::Scope(_, ScopeKind::Memory) => Err(Error::Session(
            "edit memory through the memory view instead".into(),
        )),
        Target::Scope(frame, ScopeKind::Locals) => {
            let debug = info
                .debug
                .clone()
                .ok_or_else(|| Error::Session("no debug information".into()))?;
            let pc = paused.frames[frame]
                .pc
                .ok_or_else(|| Error::Session("frame has no code location".into()))?;
            let scope = debug
                .frame_variables(pc)
                .ok_or_else(|| Error::Session("no variables here".into()))?;
            let instance = frame_instance(store, paused, frame);
            let located =
                frames::locate_variables(store, &debug, &paused.frames[frame], instance, &scope);
            let (variable, place) = located
                .into_iter()
                .rev()
                .find(|(v, _)| v.name == name)
                .ok_or_else(|| Error::Session(format!("no variable named `{name}`")))?;
            let value = TypedValue {
                ty: variable.ty,
                place,
            };
            assign_and_describe(
                store,
                info,
                paused,
                instance,
                name,
                value,
                Some(variable.name),
                text,
                hex,
            )
        }
        Target::Value(parent, path) => {
            let debug = info
                .debug
                .clone()
                .ok_or_else(|| Error::Session("no debug information".into()))?;
            let instance = paused.instance.or_else(|| frame_instance(store, paused, 0));
            let child = ValueReader {
                store,
                instance,
                debug: &debug,
            }
            .children(&parent, 0, None)?
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::Session(format!("no member named `{name}`")))?;
            let child_path = path.map(|p| format!("{p}{}", child.accessor));
            assign_and_describe(
                store,
                info,
                paused,
                instance,
                name,
                child.value,
                child_path,
                text,
                hex,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn assign_and_describe(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &mut Paused,
    instance: Option<Instance>,
    name: &str,
    value: TypedValue,
    path: Option<String>,
    text: &str,
    hex: bool,
) -> Result<VariableInfo> {
    let debug = info
        .debug
        .clone()
        .ok_or_else(|| Error::Session("no debug information".into()))?;
    ValueReader {
        store,
        instance,
        debug: &debug,
    }
    .assign(&value, text)?;
    Ok(typed_entry(
        store,
        info,
        paused,
        instance,
        name.to_string(),
        value,
        path,
        hex,
    ))
}

fn set_global(
    store: &mut StoreContextMut<'_, HostState>,
    info: &ProgramInfo,
    paused: &Paused,
    frame: usize,
    name: &str,
    text: &str,
    hex: bool,
) -> Result<VariableInfo> {
    let instance =
        frame_instance(store, paused, frame).ok_or_else(|| Error::Session("no instance".into()))?;
    let index = (0..info.module.globals.len() as u32)
        .find(|i| global_display_name(info, *i) == name)
        .ok_or_else(|| Error::Session(format!("no global named `{name}`")))?;
    let global = instance
        .debug_global(store.as_context_mut(), index)
        .ok_or_else(|| Error::Session(format!("global `{name}` is not accessible")))?;
    let ty = global.ty(store.as_context_mut());
    if ty.mutability() != Mutability::Var {
        return Err(Error::Session(format!("global `{name}` is immutable")));
    }
    let value = parse_val_str(text, ty.content())?;
    global.set(store.as_context_mut(), value)?;
    let current = global.get(store.as_context_mut());
    Ok(VariableInfo {
        name: name.to_string(),
        value: format_val(&current, hex),
        type_name: Some(val_type_name(&current).to_string()),
        evaluate_name: Some(format!("$global{index}")),
        ..VariableInfo::default()
    })
}
