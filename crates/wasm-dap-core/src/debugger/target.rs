//! The debuggee thread: owns the Wasmtime store and handles debug events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use wasmtime::{
    AsContext, AsContextMut, DebugEvent, DebugHandler, ExternType, Func, Instance, Linker, Module,
    ModulePC, Store, StoreContextMut, UpdateDeadline, Val, WasmBacktrace,
};

use super::breakpoints::{BreakpointKind, BreakpointSet, ExceptionFilters};
use super::eval::{self, Evaluator, NestedCaller, Value};
use super::frames;
use super::memory::{self, parse_memory_reference};
use super::program::{CodeSource, ProgramInfo};
use super::stepping::{Decision, Phase, StepPlan};
use super::variables::{self, Paused, VarTable};
use super::{
    Command, EntryPoint, EvalContext, EventSink, ExceptionDetails, LaunchOptions, MemoryRead,
    OutputCategory, ResumeMode, Shared, StackTrace, StopInfo, StopReason, TargetEvent,
    VariableInfo,
};
use crate::error::{Error, Result, format_runtime_error};
use crate::runtime::linker::UNRESOLVED_IMPORT;
use crate::runtime::values::{format_val, parse_val};
use crate::runtime::{HostState, OutputSink, Program, StreamKind, block_on, build_linker};

const TERMINATED: &str = "execution terminated by the debugger";
const EVALUATION_TIMEOUT: &str = "evaluation interrupted by the debugger";
const EVALUATION_LIMIT: Duration = Duration::from_secs(5);

pub(crate) struct TargetSetup {
    pub program: Program,
    pub info: Arc<ProgramInfo>,
    pub options: LaunchOptions,
    pub shared: Arc<Shared>,
    pub events: EventSink,
    pub commands: Receiver<Command>,
    pub ready: Sender<Result<()>>,
}

struct Control {
    applied: Arc<BreakpointSet>,
    hit_counts: HashMap<i64, u64>,
    step: Option<StepPlan>,
    temp_breakpoint: Option<u32>,
    /// Stop reported at the next executed instruction (entry or pause).
    pending_stop: Option<StopReason>,
    instance: Option<Instance>,
    exception: Option<ExceptionDetails>,
}

pub(crate) struct TargetState {
    info: Arc<ProgramInfo>,
    module: Module,
    shared: Arc<Shared>,
    events: EventSink,
    commands: Mutex<Receiver<Command>>,
    evaluating: AtomicU32,
    no_debug: bool,
    control: Mutex<Control>,
}

#[derive(Clone)]
struct Handler(Arc<TargetState>);

impl DebugHandler for Handler {
    type Data = HostState;

    fn handle(
        &self,
        store: StoreContextMut<'_, HostState>,
        event: DebugEvent<'_>,
    ) -> impl Future<Output = ()> + Send {
        self.0.on_event(store, &event);
        async {}
    }
}

pub(crate) fn run(setup: TargetSetup) {
    let TargetSetup {
        program,
        info,
        options,
        shared,
        events,
        commands,
        ready,
    } = setup;
    let state = Arc::new(TargetState {
        info: info.clone(),
        module: program.module.clone(),
        shared,
        events,
        commands: Mutex::new(commands),
        evaluating: AtomicU32::new(0),
        no_debug: options.no_debug,
        control: Mutex::new(Control {
            applied: Arc::new(BreakpointSet::default()),
            hit_counts: HashMap::new(),
            step: None,
            temp_breakpoint: None,
            pending_stop: None,
            instance: None,
            exception: None,
        }),
    });

    let prepared = validate_entry(&program, &options.entry).and_then(|()| {
        let mut store = state.create_store(&program, &options)?;
        let linker = build_linker(&program, &mut store)?;
        if info.module.start.is_none() {
            let instance = block_on(linker.instantiate_async(&mut store, &program.module))
                .map_err(|e| {
                    Error::Session(format!(
                        "failed to instantiate the module: {}",
                        format_runtime_error(&e)
                    ))
                })?;
            state.control().instance = Some(instance);
        }
        Ok((store, linker))
    });
    let (mut store, linker) = match prepared {
        Ok(prepared) => {
            let _ = ready.send(Ok(()));
            prepared
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    if !state.idle_loop(&mut store, true) {
        return;
    }
    state.start(&mut store, &program, &linker, &options);
    state.idle_loop(&mut store, false);
}

fn validate_entry(program: &Program, entry: &EntryPoint) -> Result<()> {
    let exported: Vec<String> = program
        .module
        .exports()
        .filter(|e| matches!(e.ty(), ExternType::Func(_)))
        .map(|e| e.name().to_string())
        .collect();
    match entry {
        EntryPoint::Instantiate => Ok(()),
        EntryPoint::Command => match program.module.get_export("_start") {
            Some(ExternType::Func(_)) => Ok(()),
            _ => Err(Error::Config(
                "the module does not export `_start`; set `invoke` to an exported function".into(),
            )),
        },
        EntryPoint::Function { name, args } => {
            let Some(ExternType::Func(ty)) = program.module.get_export(name) else {
                return Err(Error::Config(format!(
                    "the module does not export a function named `{name}` (exported functions: {})",
                    if exported.is_empty() {
                        "none".to_string()
                    } else {
                        exported.join(", ")
                    }
                )));
            };
            let params: Vec<_> = ty.params().collect();
            if params.len() != args.len() {
                return Err(Error::Config(format!(
                    "`{name}` expects {} argument(s) but `invokeArgs` has {}",
                    params.len(),
                    args.len()
                )));
            }
            for (param, arg) in params.iter().zip(args) {
                parse_val(arg, param)?;
            }
            Ok(())
        }
    }
}

fn set_single_step(store: &mut StoreContextMut<'_, HostState>, enabled: bool) {
    if store.as_context().is_single_step() == enabled {
        return;
    }
    if let Some(mut edit) = store.as_context_mut().edit_breakpoints()
        && let Err(e) = edit.single_step(enabled)
    {
        tracing::warn!("failed to change single-step mode: {e}");
    }
}

impl TargetState {
    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn instance(&self) -> Option<Instance> {
        self.control().instance
    }

    fn emit(&self, event: TargetEvent) {
        (self.events)(event);
    }

    fn output(&self, category: OutputCategory, text: String) {
        self.emit(TargetEvent::Output { category, text });
    }

    fn next_command(&self) -> Option<Command> {
        let receiver = self.commands.lock().ok()?;
        receiver.recv().ok()
    }

    fn create_store(
        self: &Arc<Self>,
        program: &Program,
        options: &LaunchOptions,
    ) -> Result<Store<HostState>> {
        let events = self.events.clone();
        let sink: OutputSink = Arc::new(move |kind, bytes| {
            events(TargetEvent::Output {
                category: match kind {
                    StreamKind::Stdout => OutputCategory::Stdout,
                    StreamKind::Stderr => OutputCategory::Stderr,
                },
                text: String::from_utf8_lossy(bytes).into_owned(),
            })
        });
        let mut wasi = options.wasi.clone();
        wasi.output = Some(sink);
        let host = HostState::new(&wasi)?;
        let mut store = Store::new(&program.engine, host);
        store.set_epoch_deadline(1);
        let shared = self.shared.clone();
        store.epoch_deadline_callback(move |_| {
            if shared.terminate_requested.load(Ordering::SeqCst) {
                return Err(wasmtime::Error::msg(TERMINATED));
            }
            if shared.abort_evaluation.load(Ordering::SeqCst) {
                return Err(wasmtime::Error::msg(EVALUATION_TIMEOUT));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_debug_handler(Handler(self.clone()));
        store.debug_register_module(&program.module)?;
        Ok(store)
    }

    /// Serves commands while no WebAssembly is running. Returns true when
    /// execution should start.
    fn idle_loop(&self, store: &mut Store<HostState>, waiting_for_start: bool) -> bool {
        let mut paused = Paused {
            frames: Vec::new(),
            vars: VarTable::default(),
            instance: self.instance(),
        };
        loop {
            let Some(command) = self.next_command() else {
                return false;
            };
            let mut context = store.as_context_mut();
            match command {
                Command::Start if waiting_for_start => return true,
                Command::Terminate => return false,
                Command::Start | Command::Resume(_) => {}
                Command::ApplyBreakpoints => self.apply_pending(&mut context),
                other => {
                    paused.instance = self.instance();
                    self.serve(&mut context, &mut paused, other);
                }
            }
        }
    }

    fn start(
        &self,
        store: &mut Store<HostState>,
        program: &Program,
        linker: &Linker<HostState>,
        options: &LaunchOptions,
    ) {
        {
            let mut context = store.as_context_mut();
            self.apply_pending(&mut context);
            if options.stop_on_entry {
                self.control().pending_stop = Some(StopReason::Entry);
                set_single_step(&mut context, true);
            }
        }
        let outcome = self.run_entry(store, program, linker, &options.entry);
        if self.shared.terminate_requested.load(Ordering::SeqCst) {
            return;
        }
        match outcome {
            Ok(results) => {
                if let EntryPoint::Function { name, args } = &options.entry {
                    let args: Vec<String> = args
                        .iter()
                        .map(|a| match a {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    let text = match results.len() {
                        0 => format!("{name}({}) returned\n", args.join(", ")),
                        1 => format!(
                            "{name}({}) = {}\n",
                            args.join(", "),
                            format_val(&results[0], false)
                        ),
                        _ => {
                            let values: Vec<String> =
                                results.iter().map(|v| format_val(v, false)).collect();
                            format!("{name}({}) = ({})\n", args.join(", "), values.join(", "))
                        }
                    };
                    self.output(OutputCategory::Console, text);
                }
                self.emit(TargetEvent::Exited { code: 0 });
            }
            Err(error) => {
                if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    self.emit(TargetEvent::Exited { code: exit.0 });
                    return;
                }
                let mut text = format!("error: {}\n", format_runtime_error(&error));
                if let Some(backtrace) = error.downcast_ref::<WasmBacktrace>() {
                    text.push_str(&self.describe_backtrace(backtrace));
                }
                self.output(OutputCategory::Stderr, text);
                self.emit(TargetEvent::Exited { code: 1 });
            }
        }
    }

    fn run_entry(
        &self,
        store: &mut Store<HostState>,
        program: &Program,
        linker: &Linker<HostState>,
        entry: &EntryPoint,
    ) -> wasmtime::Result<Vec<Val>> {
        let instance = match self.instance() {
            Some(instance) => instance,
            None => {
                let instance = block_on(linker.instantiate_async(&mut *store, &program.module))?;
                self.control().instance = Some(instance);
                instance
            }
        };
        let (func, args) = match entry {
            EntryPoint::Instantiate => return Ok(Vec::new()),
            EntryPoint::Command => (instance.get_func(&mut *store, "_start"), Vec::new()),
            EntryPoint::Function { name, args } => {
                (instance.get_func(&mut *store, name), args.clone())
            }
        };
        let func = func.ok_or_else(|| wasmtime::Error::msg("entry function is not exported"))?;
        let ty = func.ty(&*store);
        let params = ty
            .params()
            .zip(&args)
            .map(|(ty, arg)| parse_val(arg, &ty).map_err(|e| wasmtime::Error::msg(e.to_string())))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        let mut results: Vec<Val> = ty
            .results()
            .map(|r| Val::default_for_ty(&r).unwrap_or(Val::I32(0)))
            .collect();
        block_on(func.call_async(&mut *store, &params, &mut results))?;
        Ok(results)
    }

    fn describe_backtrace(&self, backtrace: &WasmBacktrace) -> String {
        let mut text = String::new();
        for (index, frame) in backtrace.frames().iter().enumerate() {
            let pc = frame.module_offset().map(|o| o as u32);
            let name = pc
                .and_then(|pc| self.info.function_name_at(pc))
                .or_else(|| frame.func_name().map(crate::module::demangle))
                .unwrap_or_else(|| format!("func[{}]", frame.func_index()));
            let location = match pc.and_then(|pc| self.info.code_source(pc)) {
                Some(CodeSource::File { path, line, column }) => {
                    format!(" at {path}:{line}:{column}")
                }
                Some(CodeSource::Wat { .. }) | None => {
                    pc.map(|pc| format!(" at {pc:#x}")).unwrap_or_default()
                }
            };
            text.push_str(&format!("  {index}: {name}{location}\n"));
        }
        text
    }

    fn on_event(&self, mut store: StoreContextMut<'_, HostState>, event: &DebugEvent<'_>) {
        if self.evaluating.load(Ordering::SeqCst) > 0
            || self.shared.terminate_requested.load(Ordering::SeqCst)
        {
            return;
        }
        self.apply_pending(&mut store);
        let stop = match event {
            DebugEvent::Breakpoint => self.on_breakpoint(&mut store),
            DebugEvent::EpochYield => {
                // The PC reported at an epoch check is imprecise, so pause at
                // the next executed instruction instead.
                if self.shared.pause_requested.swap(false, Ordering::SeqCst) {
                    self.control().pending_stop = Some(StopReason::Pause);
                    set_single_step(&mut store, true);
                }
                None
            }
            DebugEvent::Trap(trap) => {
                self.on_exception(|f| f.traps, format!("{trap:?}"), trap.to_string())
            }
            DebugEvent::HostcallError(error) => self.on_host_error(error),
            DebugEvent::Exception(_) => None,
        };
        if let Some(stop) = stop {
            self.pause_loop(&mut store, stop);
        }
    }

    fn on_host_error(&self, error: &wasmtime::Error) -> Option<StopInfo> {
        if error.downcast_ref::<wasmtime_wasi::I32Exit>().is_some() {
            return None;
        }
        let message = format_runtime_error(error);
        if message.contains(TERMINATED) || message.contains(EVALUATION_TIMEOUT) {
            return None;
        }
        let id = if message.contains(UNRESOLVED_IMPORT) {
            "UnresolvedImport"
        } else {
            "HostError"
        };
        self.on_exception(|f| f.host_errors, id.to_string(), message)
    }

    fn on_exception(
        &self,
        enabled: impl Fn(&ExceptionFilters) -> bool,
        id: String,
        description: String,
    ) -> Option<StopInfo> {
        if self.no_debug {
            return None;
        }
        let mut control = self.control();
        if !enabled(&control.applied.exceptions) {
            return None;
        }
        control.exception = Some(ExceptionDetails {
            id,
            description: description.clone(),
        });
        Some(StopInfo {
            reason: StopReason::Exception,
            description: Some(format!("Paused on exception: {description}")),
            text: Some(description),
            hit_breakpoint_ids: Vec::new(),
        })
    }

    fn on_breakpoint(&self, store: &mut StoreContextMut<'_, HostState>) -> Option<StopInfo> {
        let frames = frames::collect(store);
        let pcs: Vec<u32> = frames.iter().filter_map(|f| f.pc).collect();
        let pc = *pcs.first()?;

        let mut control = self.control();
        if let Some(reason) = control.pending_stop.take() {
            return Some(StopInfo::new(reason));
        }
        let decision = control
            .step
            .as_mut()
            .map(|plan| plan.decide(&self.info, &pcs));
        match decision {
            Some(Decision::Stop) => return Some(StopInfo::new(StopReason::Step)),
            Some(Decision::RunTo { pc: target, depth }) => {
                if let Some(plan) = control.step.as_mut() {
                    plan.phase = Phase::ReturnTo { pc: target, depth };
                }
                let previous = control.temp_breakpoint.replace(target);
                drop(control);
                self.move_temp_breakpoint(store, previous, Some(target));
                set_single_step(store, false);
                control = self.control();
            }
            Some(Decision::SingleStepAgain) => {
                let previous = control.temp_breakpoint.take();
                drop(control);
                self.move_temp_breakpoint(store, previous, None);
                set_single_step(store, true);
                control = self.control();
            }
            Some(Decision::Continue) | None => {}
        }
        let specs = control.applied.by_address.get(&pc).cloned();
        drop(control);
        let specs = specs?;

        let paused = Paused {
            frames,
            vars: VarTable::default(),
            instance: self.instance(),
        };
        let mut hit = Vec::new();
        let mut kind = None;
        for spec in specs {
            if let Some(condition) = &spec.condition {
                match self.condition_holds(store, &paused, condition) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => self.output(
                        OutputCategory::Important,
                        format!("Breakpoint condition `{condition}` could not be evaluated: {e}\n"),
                    ),
                }
            }
            let hits = {
                let mut control = self.control();
                let count = control.hit_counts.entry(spec.id).or_insert(0);
                *count += 1;
                *count
            };
            if spec.hit_condition.is_some_and(|h| !h.matches(hits)) {
                continue;
            }
            if let Some(message) = &spec.log_message {
                let mut paused_view = Paused {
                    frames: frames::collect(store),
                    vars: VarTable::default(),
                    instance: paused.instance,
                };
                let text = self.interpolate(store, &mut paused_view, message);
                self.output(OutputCategory::Console, format!("{text}\n"));
                continue;
            }
            hit.push(spec.id);
            kind.get_or_insert(spec.kind);
        }
        if hit.is_empty() {
            return None;
        }
        let reason = match kind {
            Some(BreakpointKind::Function) => StopReason::FunctionBreakpoint,
            Some(BreakpointKind::Instruction) => StopReason::InstructionBreakpoint,
            _ => StopReason::Breakpoint,
        };
        Some(StopInfo {
            reason,
            description: None,
            text: None,
            hit_breakpoint_ids: hit,
        })
    }

    fn pause_loop(&self, store: &mut StoreContextMut<'_, HostState>, stop: StopInfo) {
        self.end_step(store);
        let frames = frames::collect(store);
        let instance = frames
            .first()
            .and_then(|f| f.handle.instance(store.as_context_mut()).ok())
            .or_else(|| self.instance());
        let mut paused = Paused {
            frames,
            vars: VarTable::default(),
            instance,
        };
        if stop.reason != StopReason::Exception {
            self.control().exception = None;
        }
        self.emit(TargetEvent::Stopped(stop));
        loop {
            let Some(command) = self.next_command() else {
                self.request_termination();
                return;
            };
            match command {
                Command::Resume(mode) => {
                    // Breakpoints published while the stop was in flight must be
                    // patched before any further code runs.
                    self.apply_pending(store);
                    self.resume(store, &paused, mode);
                    return;
                }
                Command::Terminate => {
                    self.request_termination();
                    return;
                }
                Command::ApplyBreakpoints => self.apply_pending(store),
                Command::Start => {}
                other => self.serve(store, &mut paused, other),
            }
        }
    }

    fn request_termination(&self) {
        self.shared
            .terminate_requested
            .store(true, Ordering::SeqCst);
        self.shared.interrupt();
    }

    fn resume(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &Paused,
        mode: ResumeMode,
    ) {
        let ResumeMode::Step(kind, granularity) = mode else {
            return;
        };
        let pcs = paused.frame_pcs();
        let Some(plan) = StepPlan::new(&self.info, kind, granularity, &pcs) else {
            return;
        };
        match plan.phase {
            Phase::SingleStep => set_single_step(store, true),
            Phase::ReturnTo { pc, .. } => {
                self.move_temp_breakpoint(store, None, Some(pc));
                self.control().temp_breakpoint = Some(pc);
            }
        }
        self.control().step = Some(plan);
    }

    fn end_step(&self, store: &mut StoreContextMut<'_, HostState>) {
        let temp = {
            let mut control = self.control();
            control.step = None;
            control.pending_stop = None;
            control.temp_breakpoint.take()
        };
        if temp.is_some() {
            self.move_temp_breakpoint(store, temp, None);
        }
        set_single_step(store, false);
    }

    fn move_temp_breakpoint(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        old: Option<u32>,
        new: Option<u32>,
    ) {
        let Some(mut edit) = store.as_context_mut().edit_breakpoints() else {
            return;
        };
        if let Some(pc) = old {
            let _ = edit.remove_breakpoint(&self.module, ModulePC::new(pc));
        }
        if let Some(pc) = new {
            let _ = edit.add_breakpoint(&self.module, ModulePC::new(pc));
        }
    }

    /// Applies the breakpoint set most recently published by the adapter.
    fn apply_pending(&self, store: &mut StoreContextMut<'_, HostState>) {
        let pending = self
            .shared
            .pending_breakpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(mut next) = pending else {
            return;
        };
        if self.no_debug {
            next = Arc::new(BreakpointSet {
                by_address: Default::default(),
                exceptions: ExceptionFilters {
                    traps: false,
                    host_errors: false,
                },
            });
        }
        let previous = {
            let mut control = self.control();
            let previous = std::mem::replace(&mut control.applied, next.clone());
            let live: std::collections::HashSet<i64> =
                next.by_address.values().flatten().map(|s| s.id).collect();
            control.hit_counts.retain(|id, _| live.contains(id));
            previous
        };
        let Some(mut edit) = store.as_context_mut().edit_breakpoints() else {
            return;
        };
        for pc in previous.addresses() {
            if !next.by_address.contains_key(&pc) {
                let _ = edit.remove_breakpoint(&self.module, ModulePC::new(pc));
            }
        }
        for pc in next.addresses() {
            if !previous.by_address.contains_key(&pc)
                && let Err(e) = edit.add_breakpoint(&self.module, ModulePC::new(pc))
            {
                tracing::warn!("failed to set breakpoint at {pc:#x}: {e}");
            }
        }
    }

    fn serve(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &mut Paused,
        command: Command,
    ) {
        match command {
            Command::StackTrace {
                start,
                levels,
                reply,
            } => {
                let _ = reply.send(Ok(self.stack_trace(paused, start, levels)));
            }
            Command::Scopes { frame_id, reply } => {
                let _ = reply.send(variables::scopes(store, &self.info, paused, frame_id));
            }
            Command::Variables {
                reference,
                start,
                count,
                hex,
                reply,
            } => {
                let _ = reply.send(variables::variables(
                    store, &self.info, paused, reference, start, count, hex,
                ));
            }
            Command::SetVariable {
                reference,
                name,
                value,
                hex,
                reply,
            } => {
                let _ = reply.send(variables::set_variable(
                    store, &self.info, paused, reference, &name, &value, hex,
                ));
            }
            Command::Evaluate {
                expression,
                frame_id,
                context,
                hex,
                reply,
            } => {
                let allow_calls = context != EvalContext::Hover;
                let _ = reply.send(self.evaluate(
                    store,
                    paused,
                    &expression,
                    frame_id,
                    hex,
                    allow_calls,
                ));
            }
            Command::ReadMemory {
                reference,
                offset,
                count,
                reply,
            } => {
                let _ = reply.send(self.read_memory(store, paused, &reference, offset, count));
            }
            Command::WriteMemory {
                reference,
                offset,
                data,
                reply,
            } => {
                let _ = reply.send(self.write_memory(store, paused, &reference, offset, &data));
            }
            Command::ExceptionInfo { reply } => {
                let _ = reply.send(Ok(self.control().exception.clone()));
            }
            Command::Start
            | Command::ApplyBreakpoints
            | Command::Resume(_)
            | Command::Terminate => {}
        }
    }

    fn stack_trace(&self, paused: &Paused, start: usize, levels: Option<usize>) -> StackTrace {
        let total = paused.frames.len();
        let end = match levels {
            Some(levels) if levels > 0 => start.saturating_add(levels).min(total),
            _ => total,
        };
        let frames = (start.min(total)..end)
            .map(|index| frames::describe(&self.info, index, &paused.frames[index]))
            .collect();
        StackTrace { frames, total }
    }

    fn evaluate_value(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &Paused,
        expression: &str,
        frame: Option<usize>,
        allow_calls: bool,
    ) -> Result<Value> {
        let expr = crate::evaluator::parse(expression)?;
        let instance = match frame {
            Some(index) => frames::frame_instance(
                store,
                paused.frames.get(index).map(|f| &f.handle),
                paused.instance,
            ),
            None => paused.instance,
        }
        .or_else(|| self.instance());
        let mut evaluator = Evaluator {
            store,
            info: &self.info,
            paused,
            frame,
            instance,
            caller: self,
            allow_calls,
        };
        evaluator.evaluate(&expr)
    }

    fn condition_holds(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &Paused,
        condition: &str,
    ) -> Result<bool> {
        let frame = (!paused.frames.is_empty()).then_some(0);
        let value = self.evaluate_value(store, paused, condition, frame, true)?;
        let instance = paused.instance.or_else(|| self.instance());
        let mut evaluator = Evaluator {
            store,
            info: &self.info,
            paused,
            frame,
            instance,
            caller: self,
            allow_calls: true,
        };
        evaluator.truthy(value)
    }

    fn evaluate(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &mut Paused,
        expression: &str,
        frame_id: Option<i64>,
        hex: bool,
        allow_calls: bool,
    ) -> Result<VariableInfo> {
        let frame = match frame_id {
            Some(id) => Some(paused.frame_index(id)?),
            None => (!paused.frames.is_empty()).then_some(0),
        };
        let value = self.evaluate_value(store, paused, expression, frame, allow_calls)?;
        let instance = paused.instance.or_else(|| self.instance());
        Ok(match value {
            Value::Typed(typed) => variables::typed_entry(
                store,
                &self.info,
                paused,
                instance,
                expression.to_string(),
                typed,
                Some(expression.to_string()),
                hex,
            ),
            other => {
                let (value, type_name) = eval::describe_simple(&other, hex);
                let memory_reference = match &other {
                    Value::Scalar(super::values::Scalar::Pointer { address, .. }) => {
                        Some(memory::format_memory_reference(0, *address))
                    }
                    _ => None,
                };
                VariableInfo {
                    name: expression.to_string(),
                    value,
                    type_name,
                    memory_reference,
                    ..VariableInfo::default()
                }
            }
        })
    }

    fn interpolate(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &mut Paused,
        message: &str,
    ) -> String {
        let mut out = String::new();
        let mut chars = message.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '{' => {
                    let mut expression = String::new();
                    let mut depth = 1;
                    for next in chars.by_ref() {
                        match next {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        expression.push(next);
                    }
                    let frame_id = (!paused.frames.is_empty()).then_some(1);
                    match self.evaluate(store, paused, expression.trim(), frame_id, false, true) {
                        Ok(info) => out.push_str(&info.value),
                        Err(e) => out.push_str(&format!("<{e}>")),
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    fn resolve_memory(
        &self,
        paused: &Paused,
        reference: &str,
        offset: i64,
    ) -> Result<(Instance, u32, i128)> {
        let (memory, base) = parse_memory_reference(reference)
            .ok_or_else(|| Error::Session(format!("invalid memory reference `{reference}`")))?;
        let instance = paused
            .instance
            .or_else(|| self.instance())
            .ok_or_else(|| Error::Session("the module is not instantiated".into()))?;
        Ok((instance, memory, i128::from(base) + i128::from(offset)))
    }

    fn read_memory(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &Paused,
        reference: &str,
        offset: i64,
        count: usize,
    ) -> Result<MemoryRead> {
        let (instance, index, address) = self.resolve_memory(paused, reference, offset)?;
        if address < 0 {
            let skipped = usize::try_from(-address).unwrap_or(usize::MAX).min(count);
            let data = memory::read(store, instance, index, 0, count - skipped)?;
            return Ok(MemoryRead {
                address: 0,
                unreadable: count - data.len(),
                data,
            });
        }
        let address = address as u64;
        let data = memory::read(store, instance, index, address, count)?;
        Ok(MemoryRead {
            address,
            unreadable: count - data.len(),
            data,
        })
    }

    fn write_memory(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        paused: &Paused,
        reference: &str,
        offset: i64,
        data: &[u8],
    ) -> Result<usize> {
        let (instance, index, address) = self.resolve_memory(paused, reference, offset)?;
        let address = u64::try_from(address)
            .map_err(|_| Error::Session("cannot write before the start of memory".into()))?;
        memory::write(store, instance, index, address, data)?;
        Ok(data.len())
    }
}

impl NestedCaller for TargetState {
    fn call(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        func: Func,
        params: &[Val],
        results: &mut [Val],
    ) -> Result<()> {
        let was_single_stepping = store.as_context().is_single_step();
        if was_single_stepping {
            set_single_step(store, false);
        }
        self.evaluating.fetch_add(1, Ordering::SeqCst);
        let (cancel, cancelled) = mpsc::channel::<()>();
        let shared = self.shared.clone();
        let watchdog = std::thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = cancelled.recv_timeout(EVALUATION_LIMIT) {
                shared.abort_evaluation.store(true, Ordering::SeqCst);
                shared.interrupt();
            }
        });
        let outcome = block_on(func.call_async(store.as_context_mut(), params, results));
        drop(cancel);
        let _ = watchdog.join();
        let timed_out = self.shared.abort_evaluation.swap(false, Ordering::SeqCst);
        self.evaluating.fetch_sub(1, Ordering::SeqCst);
        if was_single_stepping {
            set_single_step(store, true);
        }
        outcome.map_err(|e| {
            if timed_out {
                Error::Eval(format!(
                    "the call did not finish within {} seconds",
                    EVALUATION_LIMIT.as_secs()
                ))
            } else {
                Error::Eval(format_runtime_error(&e))
            }
        })
    }
}
