//! Execution tracing.
//!
//! The tracer runs a program under Wasmtime's guest debugging API with a
//! breakpoint at the entry and at every exit (`return` and the final `end`) of
//! each function. Call arguments are read from the callee's locals and return
//! values from its operand stack. A shadow call stack recovers returns whose
//! exit instruction was not observed. Optionally every executed instruction
//! is recorded with single-stepping.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use wasmtime::{
    AsContextMut, DebugEvent, DebugHandler, FrameHandle, ModulePC, Store, StoreContextMut,
    UpdateDeadline, Val, WasmBacktrace,
};

use crate::debugger::EntryPoint;
use crate::debugger::program::{CodeSource, ProgramInfo};
use crate::dwarf::SourceMap;
use crate::error::{Error, Result, format_runtime_error};
use crate::module::InstrKind;
use crate::runtime::values::{format_val, parse_val, val_type_name};
use crate::runtime::{
    HostState, OutputSink, Program, StreamKind, WasiOptions, block_on, build_linker,
};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TraceValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub ty: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    Call {
        depth: usize,
        function: String,
        index: u32,
        args: Vec<TraceValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<String>,
    },
    Return {
        depth: usize,
        function: String,
        index: u32,
        /// `None` when the return was inferred rather than observed.
        #[serde(skip_serializing_if = "Option::is_none")]
        results: Option<Vec<TraceValue>>,
    },
    Instruction {
        depth: usize,
        pc: String,
        function: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<String>,
    },
    Output {
        stream: &'static str,
        text: String,
    },
    Trap {
        message: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        backtrace: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExitStatus {
    Returned { results: Vec<TraceValue> },
    Exited { code: i32 },
    Trapped { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub events: usize,
    pub calls: u64,
    pub instructions: u64,
    pub max_depth: usize,
    pub truncated: bool,
    pub elapsed_ms: u128,
    pub exit: ExitStatus,
}

pub struct TraceOptions {
    pub entry: EntryPoint,
    pub wasi: WasiOptions,
    pub max_events: usize,
    pub instructions: bool,
    /// Report guest output as trace events instead of inheriting stdio.
    pub capture_output: bool,
    pub source_map: SourceMap,
}

type Sink = Box<dyn FnMut(&TraceEvent) + Send>;

struct Recorder {
    info: Arc<ProgramInfo>,
    sink: Sink,
    events: usize,
    max_events: usize,
    calls: u64,
    instructions: u64,
    max_depth: usize,
    truncated: bool,
    stack: Vec<(u32, usize)>,
    single_step: bool,
    module: wasmtime::Module,
    boundaries: Vec<u32>,
    detached: bool,
}

impl Recorder {
    fn emit(&mut self, event: TraceEvent) {
        if self.events >= self.max_events {
            self.truncated = true;
            return;
        }
        self.events += 1;
        (self.sink)(&event);
    }

    fn location(&self, pc: u32) -> Option<String> {
        match self.info.code_source(pc)? {
            CodeSource::File { path, line, .. } => Some(format!("{path}:{line}")),
            CodeSource::Wat { .. } => None,
        }
    }

    fn function_name(&self, index: u32, pc: u32) -> String {
        self.info
            .function_name_at(pc)
            .unwrap_or_else(|| self.info.module.display_function_name(index))
    }

    /// Emits returns for shadow frames deeper than `depth` (or at it when `inclusive`).
    fn unwind_to(&mut self, depth: usize, inclusive: bool) {
        while let Some(&(index, d)) = self.stack.last() {
            if d > depth || (inclusive && d == depth) {
                self.stack.pop();
                let pc = self
                    .info
                    .module
                    .function(index)
                    .and_then(|f| f.body.as_ref())
                    .and_then(|b| b.first_instruction())
                    .unwrap_or(0);
                let function = self.function_name(index, pc);
                self.emit(TraceEvent::Return {
                    depth: d,
                    function,
                    index,
                    results: None,
                });
            } else {
                break;
            }
        }
    }
}

#[derive(Clone)]
struct TraceHandler(Arc<Mutex<Recorder>>);

impl DebugHandler for TraceHandler {
    type Data = HostState;

    fn handle(
        &self,
        mut store: StoreContextMut<'_, HostState>,
        event: DebugEvent<'_>,
    ) -> impl Future<Output = ()> + Send {
        if matches!(event, DebugEvent::Breakpoint)
            && let Ok(mut recorder) = self.0.lock()
        {
            on_breakpoint(&mut recorder, &mut store);
        }
        async {}
    }
}

fn value(name: Option<String>, val: &Val) -> TraceValue {
    TraceValue {
        name,
        ty: val_type_name(val).to_string(),
        value: format_val(val, false),
    }
}

fn on_breakpoint(recorder: &mut Recorder, store: &mut StoreContextMut<'_, HostState>) {
    let frames: Vec<FrameHandle> = {
        let exits: Vec<FrameHandle> = store.debug_exit_frames().collect();
        let mut all = Vec::new();
        for exit in exits {
            let mut current = Some(exit);
            while let Some(frame) = current {
                current = frame.parent(store.as_context_mut()).ok().flatten();
                all.push(frame);
            }
        }
        all
    };
    let Some(top) = frames.first() else {
        return;
    };
    let Some(pc) = top
        .wasm_function_index_and_pc(store.as_context_mut())
        .ok()
        .flatten()
        .map(|(_, pc)| pc.raw())
    else {
        return;
    };
    let depth = frames.len();
    recorder.max_depth = recorder.max_depth.max(depth);
    let info = recorder.info.clone();
    let Some(function) = info.module.function_at(pc) else {
        return;
    };
    let index = function.index;
    let Some(body) = function.body.as_ref() else {
        return;
    };
    let is_entry = body.first_instruction() == Some(pc);
    let kind = info.module.instruction_kind(pc);
    let is_exit = body.last_instruction() == Some(pc) || kind == Some(InstrKind::Return);

    if recorder.single_step {
        recorder.instructions += 1;
        let text = info
            .disassembly()
            .ok()
            .and_then(|d| d.instruction_text(pc).map(str::to_string))
            .unwrap_or_default();
        let function_name = recorder.function_name(index, pc);
        let location = recorder.location(pc);
        recorder.emit(TraceEvent::Instruction {
            depth,
            pc: format!("0x{pc:08x}"),
            function: function_name,
            text,
            location,
        });
    }

    if is_entry {
        recorder.unwind_to(depth, true);
        recorder.calls += 1;
        let sig = info.module.func_sig(index);
        let params = sig.map(|s| s.params.len()).unwrap_or(0) as u32;
        let args = (0..params)
            .filter_map(|i| {
                let val = top.local(store.as_context_mut(), i).ok()?;
                let name = info.module.local_name(index, i).map(str::to_string);
                Some(value(name, &val))
            })
            .collect();
        let function_name = recorder.function_name(index, pc);
        let location = recorder.location(pc);
        recorder.stack.push((index, depth));
        recorder.emit(TraceEvent::Call {
            depth,
            function: function_name,
            index,
            args,
            location,
        });
    }

    if is_exit {
        recorder.unwind_to(depth, false);
        if recorder.stack.last() == Some(&(index, depth)) {
            recorder.stack.pop();
            let results_count = info
                .module
                .func_sig(index)
                .map(|s| s.results.len())
                .unwrap_or(0) as u32;
            let stack_depth = top.num_stacks(store.as_context_mut()).unwrap_or(0);
            let results = (stack_depth.saturating_sub(results_count)..stack_depth)
                .filter_map(|i| top.stack(store.as_context_mut(), i).ok())
                .map(|v| value(None, &v))
                .collect();
            let function_name = recorder.function_name(index, pc);
            recorder.emit(TraceEvent::Return {
                depth,
                function: function_name,
                index,
                results: Some(results),
            });
        }
    }

    // Once the event limit is reached, stop observing so the program runs at full speed.
    if recorder.truncated && !recorder.detached {
        recorder.detached = true;
        if let Some(mut edit) = store.as_context_mut().edit_breakpoints() {
            let _ = edit.single_step(false);
            for &pc in &recorder.boundaries {
                let _ = edit.remove_breakpoint(&recorder.module, ModulePC::new(pc));
            }
        }
    }
}

/// Runs the program and reports trace events to `sink` as they happen.
pub fn trace(
    program: Program,
    options: TraceOptions,
    sink: impl FnMut(&TraceEvent) + Send + 'static,
) -> Result<TraceSummary> {
    let started = Instant::now();
    let info = Arc::new(ProgramInfo::new(
        &program.path,
        program.info.clone(),
        program.debug.clone(),
        options.source_map.clone(),
    ));
    let recorder = Arc::new(Mutex::new(Recorder {
        info: info.clone(),
        sink: Box::new(sink),
        events: 0,
        max_events: options.max_events,
        calls: 0,
        instructions: 0,
        max_depth: 0,
        truncated: false,
        stack: Vec::new(),
        single_step: options.instructions,
        module: program.module.clone(),
        boundaries: boundary_offsets(&program),
        detached: false,
    }));

    let mut wasi = options.wasi.clone();
    if options.capture_output {
        let recorder = recorder.clone();
        let sink: OutputSink = Arc::new(move |kind, bytes| {
            if let Ok(mut recorder) = recorder.lock() {
                recorder.emit(TraceEvent::Output {
                    stream: match kind {
                        StreamKind::Stdout => "stdout",
                        StreamKind::Stderr => "stderr",
                    },
                    text: String::from_utf8_lossy(bytes).into_owned(),
                });
            }
        });
        wasi.output = Some(sink);
    }
    let mut store = Store::new(&program.engine, HostState::new(&wasi)?);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Continue(1)));
    store.set_debug_handler(TraceHandler(recorder.clone()));
    store.debug_register_module(&program.module)?;
    {
        let mut edit = store
            .edit_breakpoints()
            .ok_or_else(|| Error::Session("guest debugging is not enabled".into()))?;
        for pc in boundary_offsets(&program) {
            edit.add_breakpoint(&program.module, ModulePC::new(pc))?;
        }
        if options.instructions {
            edit.single_step(true)?;
        }
    }
    let linker = build_linker(&program, &mut store)?;

    let outcome = (|| -> wasmtime::Result<Vec<Val>> {
        let instance = block_on(linker.instantiate_async(&mut store, &program.module))?;
        let (func, args) = match &options.entry {
            EntryPoint::Instantiate => return Ok(Vec::new()),
            EntryPoint::Command => (instance.get_func(&mut store, "_start"), Vec::new()),
            EntryPoint::Function { name, args } => {
                (instance.get_func(&mut store, name), args.clone())
            }
        };
        let func = func.ok_or_else(|| wasmtime::Error::msg("entry function is not exported"))?;
        let ty = func.ty(&store);
        if ty.params().len() != args.len() {
            return Err(wasmtime::Error::msg(format!(
                "the entry function expects {} argument(s), got {}",
                ty.params().len(),
                args.len()
            )));
        }
        let params = ty
            .params()
            .zip(&args)
            .map(|(t, a)| parse_val(a, &t).map_err(|e| wasmtime::Error::msg(e.to_string())))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        let mut results: Vec<Val> = ty
            .results()
            .map(|r| Val::default_for_ty(&r).unwrap_or(Val::I32(0)))
            .collect();
        block_on(func.call_async(&mut store, &params, &mut results))?;
        Ok(results)
    })();

    let mut recorder = recorder
        .lock()
        .map_err(|_| Error::Session("tracer state poisoned".into()))?;
    let exit = match outcome {
        Ok(results) => {
            recorder.unwind_to(0, false);
            ExitStatus::Returned {
                results: results.iter().map(|v| value(None, v)).collect(),
            }
        }
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                recorder.stack.clear();
                ExitStatus::Exited { code: exit.0 }
            } else {
                let message = format_runtime_error(&error);
                let backtrace = error
                    .downcast_ref::<WasmBacktrace>()
                    .map(|bt| {
                        bt.frames()
                            .iter()
                            .map(|frame| {
                                let pc = frame.module_offset().map(|o| o as u32);
                                let name =
                                    pc.and_then(|pc| info.function_name_at(pc)).unwrap_or_else(
                                        || info.module.display_function_name(frame.func_index()),
                                    );
                                match pc.and_then(|pc| recorder.location(pc)) {
                                    Some(location) => format!("{name} at {location}"),
                                    None => name,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                recorder.stack.clear();
                recorder.emit(TraceEvent::Trap {
                    message: message.clone(),
                    backtrace,
                });
                ExitStatus::Trapped { message }
            }
        }
    };
    Ok(TraceSummary {
        events: recorder.events,
        calls: recorder.calls,
        instructions: recorder.instructions,
        max_depth: recorder.max_depth,
        truncated: recorder.truncated,
        elapsed_ms: started.elapsed().as_millis(),
        exit,
    })
}

impl TraceSummary {
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms as u64)
    }
}

/// Entry and exit instructions of every defined function.
fn boundary_offsets(program: &Program) -> Vec<u32> {
    let mut offsets = Vec::new();
    for function in program.info.defined_functions() {
        let Some(body) = &function.body else {
            continue;
        };
        for instruction in &body.instructions {
            let is_boundary = body.first_instruction() == Some(instruction.offset)
                || body.last_instruction() == Some(instruction.offset)
                || instruction.kind == InstrKind::Return;
            if is_boundary {
                offsets.push(instruction.offset);
            }
        }
    }
    offsets
}
