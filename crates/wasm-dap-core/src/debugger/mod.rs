//! The debugging engine.
//!
//! A debuggee runs on its own thread inside a Wasmtime store with guest
//! debugging enabled. Wasmtime reports breakpoints, single steps, epoch
//! interruptions and traps to a debug handler on that thread; while the
//! debuggee is paused the handler blocks and serves inspection commands sent
//! by [`Debugger`], which lives on the adapter thread.

pub mod breakpoints;
mod eval;
mod frames;
pub mod memory;
pub mod program;
mod stepping;
mod target;
mod values;
mod variables;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::Engine;

pub use breakpoints::{
    BreakpointManager, BreakpointSet, BreakpointStatus, ExceptionFilters,
    FunctionBreakpointRequest, InstructionBreakpointRequest, SourceBreakpointRequest, SourceTarget,
};
pub use program::{CodeSource, ProgramInfo};

use crate::error::{Error, Result};
use crate::runtime::{Program, WasiOptions};

/// How the debuggee is started once configuration is done.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryPoint {
    /// Call the WASI command entry point `_start`.
    Command,
    /// Call an exported function with arguments parsed by parameter type.
    Function {
        name: String,
        args: Vec<serde_json::Value>,
    },
    /// Only instantiate the module, which runs its start function.
    Instantiate,
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub entry: EntryPoint,
    pub wasi: WasiOptions,
    pub stop_on_entry: bool,
    pub no_debug: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Entry,
    Breakpoint,
    FunctionBreakpoint,
    InstructionBreakpoint,
    Step,
    Pause,
    Exception,
}

impl StopReason {
    pub fn as_dap(self) -> &'static str {
        match self {
            StopReason::Entry => "entry",
            StopReason::Breakpoint => "breakpoint",
            StopReason::FunctionBreakpoint => "function breakpoint",
            StopReason::InstructionBreakpoint => "instruction breakpoint",
            StopReason::Step => "step",
            StopReason::Pause => "pause",
            StopReason::Exception => "exception",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StopInfo {
    pub reason: StopReason,
    pub description: Option<String>,
    pub text: Option<String>,
    pub hit_breakpoint_ids: Vec<i64>,
}

impl StopInfo {
    pub(crate) fn new(reason: StopReason) -> Self {
        Self {
            reason,
            description: None,
            text: None,
            hit_breakpoint_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCategory {
    Stdout,
    Stderr,
    Console,
    Important,
}

impl OutputCategory {
    pub fn as_dap(self) -> &'static str {
        match self {
            OutputCategory::Stdout => "stdout",
            OutputCategory::Stderr => "stderr",
            OutputCategory::Console => "console",
            OutputCategory::Important => "important",
        }
    }
}

/// Notification from the debuggee thread.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetEvent {
    Stopped(StopInfo),
    Output {
        category: OutputCategory,
        text: String,
    },
    Exited {
        code: i32,
    },
}

pub type EventSink = Arc<dyn Fn(TargetEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    In,
    Over,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Line,
    Instruction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    Continue,
    Step(StepKind, Granularity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInfo {
    pub id: i64,
    pub name: String,
    pub pc: Option<u32>,
    pub function_index: Option<u32>,
    pub source: Option<CodeSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackTrace {
    pub frames: Vec<FrameInfo>,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    pub name: String,
    pub reference: i64,
    pub hint: Option<&'static str>,
    pub named: Option<usize>,
    pub indexed: Option<usize>,
    pub expensive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VariableInfo {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    pub reference: i64,
    pub named: Option<usize>,
    pub indexed: Option<usize>,
    pub memory_reference: Option<String>,
    pub evaluate_name: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalContext {
    Watch,
    Repl,
    Hover,
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRead {
    pub address: u64,
    pub data: Vec<u8>,
    pub unreadable: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionDetails {
    pub id: String,
    pub description: String,
}

/// Lifecycle of the debuggee as observed by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    NotStarted,
    Running,
    Stopped,
    Exited,
}

type Reply<T> = Sender<Result<T>>;

pub(crate) enum Command {
    Start,
    ApplyBreakpoints,
    Resume(ResumeMode),
    StackTrace {
        start: usize,
        levels: Option<usize>,
        reply: Reply<StackTrace>,
    },
    Scopes {
        frame_id: i64,
        reply: Reply<Vec<ScopeInfo>>,
    },
    Variables {
        reference: i64,
        start: Option<usize>,
        count: Option<usize>,
        hex: bool,
        reply: Reply<Vec<VariableInfo>>,
    },
    SetVariable {
        reference: i64,
        name: String,
        value: String,
        hex: bool,
        reply: Reply<VariableInfo>,
    },
    Evaluate {
        expression: String,
        frame_id: Option<i64>,
        context: EvalContext,
        hex: bool,
        reply: Reply<VariableInfo>,
    },
    ReadMemory {
        reference: String,
        offset: i64,
        count: usize,
        reply: Reply<MemoryRead>,
    },
    WriteMemory {
        reference: String,
        offset: i64,
        data: Vec<u8>,
        reply: Reply<usize>,
    },
    ExceptionInfo {
        reply: Reply<Option<ExceptionDetails>>,
    },
    Terminate,
}

/// State shared between the adapter thread and the debuggee thread.
pub(crate) struct Shared {
    pub engine: Engine,
    pub pause_requested: AtomicBool,
    pub terminate_requested: AtomicBool,
    pub abort_evaluation: AtomicBool,
    pub pending_breakpoints: Mutex<Option<Arc<BreakpointSet>>>,
}

impl Shared {
    /// Makes running WebAssembly observe the shared flags at its next
    /// function entry or loop back-edge.
    pub fn interrupt(&self) {
        self.engine.increment_epoch();
    }
}

const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Adapter-side handle to a debuggee.
pub struct Debugger {
    program: Arc<ProgramInfo>,
    shared: Arc<Shared>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    state: RunState,
}

impl std::fmt::Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debugger")
            .field("program", &self.program.path)
            .field("state", &self.state)
            .finish()
    }
}

impl Debugger {
    /// Prepares the debuggee: creates its store, provides imports and
    /// instantiates the module unless it has a start function (which then
    /// runs under the debugger once execution starts).
    pub fn launch(
        program: Program,
        options: LaunchOptions,
        source_map: crate::dwarf::SourceMap,
        events: EventSink,
    ) -> Result<Self> {
        let info = Arc::new(ProgramInfo::new(
            &program.path,
            program.info.clone(),
            program.debug.clone(),
            source_map,
        ));
        let shared = Arc::new(Shared {
            engine: program.engine.clone(),
            pause_requested: AtomicBool::new(false),
            terminate_requested: AtomicBool::new(false),
            abort_evaluation: AtomicBool::new(false),
            pending_breakpoints: Mutex::new(None),
        });
        let (commands_tx, commands_rx): (Sender<Command>, Receiver<Command>) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let setup = target::TargetSetup {
            program,
            info: info.clone(),
            options,
            shared: shared.clone(),
            events,
            commands: commands_rx,
            ready: ready_tx,
        };
        let thread = std::thread::Builder::new()
            .name("wasm-dap-debuggee".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || target::run(setup))
            .map_err(Error::Io)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                program: info,
                shared,
                commands: commands_tx,
                thread: Some(thread),
                state: RunState::NotStarted,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::Session(
                    "the debuggee thread exited during launch".into(),
                ))
            }
        }
    }

    pub fn program(&self) -> &Arc<ProgramInfo> {
        &self.program
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    /// Updates the observed state from a debuggee event.
    pub fn observe(&mut self, event: &TargetEvent) {
        match event {
            TargetEvent::Stopped(_) => self.state = RunState::Stopped,
            TargetEvent::Exited { .. } => self.state = RunState::Exited,
            TargetEvent::Output { .. } => {}
        }
    }

    /// Starts execution with the given breakpoints.
    pub fn start(&mut self, breakpoints: Arc<BreakpointSet>) -> Result<()> {
        if self.state != RunState::NotStarted {
            return Err(Error::Session(
                "the program has already been started".into(),
            ));
        }
        *self.pending() = Some(breakpoints);
        self.send(Command::Start)?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Replaces the active breakpoints, interrupting running code if needed.
    pub fn update_breakpoints(&mut self, breakpoints: Arc<BreakpointSet>) {
        *self.pending() = Some(breakpoints);
        match self.state {
            RunState::Running => self.shared.interrupt(),
            RunState::Stopped | RunState::NotStarted => {
                let _ = self.send(Command::ApplyBreakpoints);
            }
            RunState::Exited => {}
        }
    }

    pub fn resume(&mut self, mode: ResumeMode) -> Result<()> {
        if self.state != RunState::Stopped {
            return Err(Error::Session("the program is not paused".into()));
        }
        self.send(Command::Resume(mode))?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Asks running code to stop at the next function entry or loop iteration.
    pub fn pause(&self) -> Result<()> {
        if self.state != RunState::Running {
            return Err(Error::Session("the program is not running".into()));
        }
        self.shared.pause_requested.store(true, Ordering::SeqCst);
        self.shared.interrupt();
        Ok(())
    }

    pub fn stack_trace(&self, start: usize, levels: Option<usize>) -> Result<StackTrace> {
        self.require_paused()?;
        self.request(|reply| Command::StackTrace {
            start,
            levels,
            reply,
        })
    }

    pub fn scopes(&self, frame_id: i64) -> Result<Vec<ScopeInfo>> {
        self.require_paused()?;
        self.request(|reply| Command::Scopes { frame_id, reply })
    }

    pub fn variables(
        &self,
        reference: i64,
        start: Option<usize>,
        count: Option<usize>,
        hex: bool,
    ) -> Result<Vec<VariableInfo>> {
        self.require_inspectable()?;
        self.request(|reply| Command::Variables {
            reference,
            start,
            count,
            hex,
            reply,
        })
    }

    pub fn set_variable(
        &self,
        reference: i64,
        name: &str,
        value: &str,
        hex: bool,
    ) -> Result<VariableInfo> {
        self.require_paused()?;
        self.request(|reply| Command::SetVariable {
            reference,
            name: name.to_string(),
            value: value.to_string(),
            hex,
            reply,
        })
    }

    pub fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<i64>,
        context: EvalContext,
        hex: bool,
    ) -> Result<VariableInfo> {
        self.require_inspectable()?;
        self.request(|reply| Command::Evaluate {
            expression: expression.to_string(),
            frame_id,
            context,
            hex,
            reply,
        })
    }

    pub fn read_memory(&self, reference: &str, offset: i64, count: usize) -> Result<MemoryRead> {
        self.require_inspectable()?;
        self.request(|reply| Command::ReadMemory {
            reference: reference.to_string(),
            offset,
            count,
            reply,
        })
    }

    pub fn write_memory(&self, reference: &str, offset: i64, data: Vec<u8>) -> Result<usize> {
        self.require_inspectable()?;
        self.request(|reply| Command::WriteMemory {
            reference: reference.to_string(),
            offset,
            data,
            reply,
        })
    }

    pub fn exception_info(&self) -> Result<Option<ExceptionDetails>> {
        self.require_paused()?;
        self.request(|reply| Command::ExceptionInfo { reply })
    }

    /// Stops the debuggee and waits briefly for its thread to finish.
    pub fn terminate(&mut self) {
        self.shared
            .terminate_requested
            .store(true, Ordering::SeqCst);
        let _ = self.commands.send(Command::Terminate);
        self.shared.interrupt();
        if let Some(thread) = self.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        self.state = RunState::Exited;
    }

    pub fn path(&self) -> PathBuf {
        self.program.path.clone()
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<Arc<BreakpointSet>>> {
        self.shared
            .pending_breakpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn require_paused(&self) -> Result<()> {
        match self.state {
            RunState::Stopped => Ok(()),
            RunState::Running => Err(Error::Session("the program is running".into())),
            RunState::NotStarted => Err(Error::Session("the program has not started".into())),
            RunState::Exited => Err(Error::Session("the program has exited".into())),
        }
    }

    fn require_inspectable(&self) -> Result<()> {
        match self.state {
            RunState::Running => Err(Error::Session("the program is running".into())),
            _ => Ok(()),
        }
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::Session("the debuggee is no longer running".into()))
    }

    fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.send(make(tx))?;
        rx.recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| Error::Session("the debuggee did not respond".into()))?
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.terminate();
        }
    }
}
