//! Debug Adapter Protocol server.
//!
//! Client requests are read on a dedicated thread and merged with debuggee
//! events into one queue, so responses and events are written in a
//! consistent order from a single thread.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};

use base64::Engine as _;
use serde_json::{Value, json};
use wasm_dap_protocol::message::{Event, Message, Request, Response};
use wasm_dap_protocol::requests::{
    BreakpointLocationsArguments, DisassembleArguments, EvaluateArguments, InitializeArguments,
    ReadMemoryArguments, ScopesArguments, SetBreakpointsArguments,
    SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments,
    SetInstructionBreakpointsArguments, SetVariableArguments, SourceArguments, StackTraceArguments,
    StepArguments, VariablesArguments, WriteMemoryArguments,
};
use wasm_dap_protocol::transport::{FrameReader, FrameWriter};
use wasm_dap_protocol::types::{
    Breakpoint, BreakpointLocation, Capabilities, DisassembledInstruction,
    ExceptionBreakpointsFilter, Module, Scope, Source, StackFrame, Thread, Variable,
    VariablePresentationHint,
};

use crate::debugger::breakpoints::{FILTER_HOST_ERRORS, FILTER_TRAPS};
use crate::debugger::memory::{format_code_address, parse_address};
use crate::debugger::{
    BreakpointManager, BreakpointStatus, CodeSource, Debugger, EvalContext,
    FunctionBreakpointRequest, Granularity, InstructionBreakpointRequest, ProgramInfo, ResumeMode,
    RunState, SourceBreakpointRequest, SourceTarget, StepKind, TargetEvent, VariableInfo,
};
use crate::dwarf::paths;
use crate::error::{Error, Result};
use crate::launch::LaunchArguments;

/// `sourceReference` of the module's text-format disassembly.
pub const WAT_SOURCE_REFERENCE: i64 = 1;
const THREAD_ID: i64 = 1;
const MODULE_ID: i64 = 1;

enum Incoming {
    Message(Message),
    Target(TargetEvent),
    Closed,
}

struct Session {
    debugger: Debugger,
    breakpoints: BreakpointManager,
    terminated_sent: bool,
}

/// A DAP server speaking over a pair of byte streams (typically stdio).
pub struct Server<R, W> {
    reader: Option<R>,
    writer: FrameWriter<W>,
    seq: i64,
    lines_start_at1: bool,
    columns_start_at1: bool,
    session: Option<Session>,
    events: Option<Sender<Incoming>>,
    /// Secret that clients must present as `serverToken` before launching.
    required_token: Option<String>,
    authenticated: bool,
}

type Body = Option<Value>;

impl<R: BufRead + Send + 'static, W: Write> Server<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Some(reader),
            writer: FrameWriter::new(writer),
            seq: 0,
            lines_start_at1: true,
            columns_start_at1: true,
            session: None,
            events: None,
            required_token: None,
            authenticated: false,
        }
    }

    /// Requires clients to send `token` as `serverToken` in the `launch`
    /// arguments; used when the server is reachable over the network.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.required_token = Some(token.into());
        self
    }

    /// Serves requests until the client disconnects or closes the stream.
    pub fn run(&mut self) -> Result<()> {
        let reader = self
            .reader
            .take()
            .ok_or_else(|| Error::Session("the server has already run".into()))?;
        let (tx, rx) = mpsc::channel();
        let reader_tx = tx.clone();
        std::thread::Builder::new()
            .name("dap-reader".into())
            .spawn(move || {
                let mut reader = FrameReader::new(reader);
                loop {
                    match reader.read_message() {
                        Ok(Some(message)) => {
                            if reader_tx.send(Incoming::Message(message)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = reader_tx.send(Incoming::Closed);
                            break;
                        }
                        Err(e) => {
                            tracing::error!("failed to read a DAP message: {e}");
                            let _ = reader_tx.send(Incoming::Closed);
                            break;
                        }
                    }
                }
            })
            .map_err(Error::Io)?;
        self.events = Some(tx);

        while let Ok(incoming) = rx.recv() {
            match incoming {
                Incoming::Message(Message::Request(request)) => {
                    if self.handle(request)? {
                        break;
                    }
                }
                Incoming::Message(_) => {}
                Incoming::Target(event) => self.on_target_event(event)?,
                Incoming::Closed => break,
            }
        }
        if let Some(mut session) = self.session.take() {
            session.debugger.terminate();
        }
        Ok(())
    }

    /// Handles one request; returns true when the session should end.
    fn handle(&mut self, request: Request) -> Result<bool> {
        if self.required_token.is_some()
            && !self.authenticated
            && !matches!(
                request.command.as_str(),
                "initialize" | "launch" | "disconnect"
            )
        {
            let error = Error::Session(
                "authentication required: add \"serverToken\" to the launch configuration".into(),
            );
            self.respond(&request, Err(error))?;
            return Ok(false);
        }
        let outcome = match request.command.as_str() {
            "initialize" => self.initialize(&request),
            "launch" => {
                if !self.check_token(&request) {
                    let error = Error::Session("invalid or missing \"serverToken\"".into());
                    self.respond(&request, Err(error))?;
                    return Ok(true);
                }
                self.launch(&request)
            }
            "attach" => Err(Error::Session(
                "attaching is not supported; use a `launch` configuration".into(),
            )),
            "configurationDone" => self.configuration_done(),
            "setBreakpoints" => self.set_breakpoints(&request),
            "setFunctionBreakpoints" => self.set_function_breakpoints(&request),
            "setInstructionBreakpoints" => self.set_instruction_breakpoints(&request),
            "setExceptionBreakpoints" => self.set_exception_breakpoints(&request),
            "breakpointLocations" => self.breakpoint_locations(&request),
            "threads" => Ok(Some(json!({
                "threads": [Thread { id: THREAD_ID, name: "main".into() }]
            }))),
            "stackTrace" => self.stack_trace(&request),
            "scopes" => self.scopes(&request),
            "variables" => self.variables(&request),
            "setVariable" => self.set_variable(&request),
            "evaluate" => self.evaluate(&request),
            "continue" => self
                .resume(ResumeMode::Continue)
                .map(|()| Some(json!({ "allThreadsContinued": true }))),
            "next" => self.step(&request, StepKind::Over),
            "stepIn" => self.step(&request, StepKind::In),
            "stepOut" => self.step(&request, StepKind::Out),
            "pause" => self.session()?.debugger.pause().map(|()| None),
            "readMemory" => self.read_memory(&request),
            "writeMemory" => self.write_memory(&request),
            "disassemble" => self.disassemble(&request),
            "loadedSources" => self.loaded_sources(),
            "source" => self.source(&request),
            "modules" => self.modules(),
            "exceptionInfo" => self.exception_info(),
            "cancel" => Ok(None),
            "terminate" => {
                self.respond(&request, Ok(None))?;
                self.terminate_session()?;
                return Ok(false);
            }
            "disconnect" => {
                if let Some(mut session) = self.session.take() {
                    session.debugger.terminate();
                }
                self.respond(&request, Ok(None))?;
                return Ok(true);
            }
            other => Err(Error::Session(format!("unsupported request `{other}`"))),
        };
        let after_launch = request.command == "launch" && outcome.is_ok();
        self.respond(&request, outcome)?;
        if after_launch {
            self.announce_module()?;
            self.send_event("initialized", None)?;
        }
        Ok(false)
    }

    fn session(&self) -> Result<&Session> {
        self.session
            .as_ref()
            .ok_or_else(|| Error::Session("no program has been launched".into()))
    }

    fn session_mut(&mut self) -> Result<&mut Session> {
        self.session
            .as_mut()
            .ok_or_else(|| Error::Session("no program has been launched".into()))
    }

    fn program(&self) -> Result<Arc<ProgramInfo>> {
        Ok(self.session()?.debugger.program().clone())
    }

    fn initialize(&mut self, request: &Request) -> Result<Body> {
        let args: InitializeArguments = parse_args(request)?;
        self.lines_start_at1 = args.lines_start_at1.unwrap_or(true);
        self.columns_start_at1 = args.columns_start_at1.unwrap_or(true);
        let capabilities = Capabilities {
            exception_breakpoint_filters: vec![
                ExceptionBreakpointsFilter {
                    filter: FILTER_TRAPS.into(),
                    label: "WebAssembly traps".into(),
                    description: Some(
                        "Pause when execution traps (unreachable, out-of-bounds access, \
                         division by zero, stack overflow, ...)"
                            .into(),
                    ),
                    default: true,
                },
                ExceptionBreakpointsFilter {
                    filter: FILTER_HOST_ERRORS.into(),
                    label: "Host function errors".into(),
                    description: Some(
                        "Pause when an imported host function fails, including calls to \
                         imports that were not provided"
                            .into(),
                    ),
                    default: true,
                },
            ],
            ..Capabilities::default()
        };
        Ok(Some(serde_json::to_value(capabilities)?))
    }

    fn launch(&mut self, request: &Request) -> Result<Body> {
        if self.session.is_some() {
            return Err(Error::Session("a program is already being debugged".into()));
        }
        let args = LaunchArguments::from_json(request.arguments.as_ref())?;
        let prepared = args.prepare()?;
        let tx = self
            .events
            .clone()
            .ok_or_else(|| Error::Session("server is not running".into()))?;
        let sink = Arc::new(move |event| {
            let _ = tx.send(Incoming::Target(event));
        });
        let debugger = Debugger::launch(
            prepared.program,
            prepared.options,
            prepared.source_map,
            sink,
        )?;
        let mut breakpoints = BreakpointManager::new();
        breakpoints
            .set_exception_filters(&[FILTER_TRAPS.to_string(), FILTER_HOST_ERRORS.to_string()]);
        self.session = Some(Session {
            debugger,
            breakpoints,
            terminated_sent: false,
        });
        Ok(None)
    }

    fn announce_module(&mut self) -> Result<()> {
        let module = self.module_description()?;
        self.send_event("module", Some(json!({ "reason": "new", "module": module })))
    }

    fn module_description(&self) -> Result<Module> {
        let program = self.program()?;
        let status = if program.debug.is_some() {
            "DWARF debug information loaded"
        } else if program.module.names.functions.is_empty() {
            "No debug information"
        } else {
            "Function names only (no DWARF)"
        };
        Ok(Module {
            id: MODULE_ID,
            name: program.module_name(),
            path: Some(native_path(&program.path.to_string_lossy())),
            symbol_status: Some(status.into()),
        })
    }

    fn configuration_done(&mut self) -> Result<Body> {
        let session = self.session_mut()?;
        let snapshot = session.breakpoints.snapshot();
        session.debugger.start(snapshot)?;
        Ok(None)
    }

    fn line_in(&self, line: u32) -> u32 {
        if self.lines_start_at1 { line } else { line + 1 }
    }

    fn line_out(&self, line: u32) -> u32 {
        if self.lines_start_at1 {
            line
        } else {
            line.saturating_sub(1)
        }
    }

    fn column_out(&self, column: u32) -> u32 {
        if self.columns_start_at1 {
            column
        } else {
            column.saturating_sub(1)
        }
    }

    fn source_target(source: &Source) -> Result<SourceTarget> {
        if source.source_reference == Some(WAT_SOURCE_REFERENCE) {
            return Ok(SourceTarget::Wat);
        }
        match &source.path {
            Some(path) => Ok(SourceTarget::File(path.clone())),
            None => Err(Error::Session(
                "the source has neither a path nor a known reference".into(),
            )),
        }
    }

    fn breakpoint_body(&self, status: &BreakpointStatus, source: Option<Source>) -> Breakpoint {
        Breakpoint {
            id: Some(status.id),
            verified: status.verified,
            message: status.message.clone(),
            source,
            line: status.line.map(|l| self.line_out(l)),
            column: status.column.map(|c| self.column_out(c)),
            instruction_reference: status.instruction_reference.clone(),
            reason: (!status.verified).then(|| "failed".to_string()),
        }
    }

    fn set_breakpoints(&mut self, request: &Request) -> Result<Body> {
        let args: SetBreakpointsArguments = parse_args(request)?;
        let target = Self::source_target(&args.source)?;
        let requests: Vec<SourceBreakpointRequest> = match (&args.breakpoints, &args.lines) {
            (Some(list), _) => list
                .iter()
                .map(|b| SourceBreakpointRequest {
                    line: self.line_in(b.line),
                    column: b.column,
                    condition: b.condition.clone(),
                    hit_condition: b.hit_condition.clone(),
                    log_message: b.log_message.clone(),
                })
                .collect(),
            (None, Some(lines)) => lines
                .iter()
                .map(|l| SourceBreakpointRequest {
                    line: self.line_in(*l),
                    ..Default::default()
                })
                .collect(),
            (None, None) => Vec::new(),
        };
        let program = self.program()?;
        let session = self.session_mut()?;
        let statuses = session.breakpoints.set_source(&program, &target, &requests);
        let snapshot = session.breakpoints.snapshot();
        session.debugger.update_breakpoints(snapshot);
        let breakpoints: Vec<Breakpoint> = statuses
            .iter()
            .map(|s| self.breakpoint_body(s, Some(args.source.clone())))
            .collect();
        Ok(Some(json!({ "breakpoints": breakpoints })))
    }

    fn set_function_breakpoints(&mut self, request: &Request) -> Result<Body> {
        let args: SetFunctionBreakpointsArguments = parse_args(request)?;
        let requests: Vec<FunctionBreakpointRequest> = args
            .breakpoints
            .into_iter()
            .map(|b| FunctionBreakpointRequest {
                name: b.name,
                condition: b.condition,
                hit_condition: b.hit_condition,
            })
            .collect();
        let program = self.program()?;
        let session = self.session_mut()?;
        let statuses = session.breakpoints.set_functions(&program, &requests);
        let snapshot = session.breakpoints.snapshot();
        session.debugger.update_breakpoints(snapshot);
        let breakpoints: Vec<Breakpoint> = statuses
            .iter()
            .map(|s| {
                let source = s
                    .instruction_reference
                    .as_deref()
                    .and_then(parse_address)
                    .and_then(|pc| self.code_source_object(&program, pc as u32));
                self.breakpoint_body(s, source)
            })
            .collect();
        Ok(Some(json!({ "breakpoints": breakpoints })))
    }

    fn set_instruction_breakpoints(&mut self, request: &Request) -> Result<Body> {
        let args: SetInstructionBreakpointsArguments = parse_args(request)?;
        let requests: Vec<InstructionBreakpointRequest> = args
            .breakpoints
            .into_iter()
            .map(|b| InstructionBreakpointRequest {
                instruction_reference: b.instruction_reference,
                offset: b.offset.unwrap_or(0),
                condition: b.condition,
                hit_condition: b.hit_condition,
            })
            .collect();
        let program = self.program()?;
        let session = self.session_mut()?;
        let statuses = session.breakpoints.set_instructions(&program, &requests);
        let snapshot = session.breakpoints.snapshot();
        session.debugger.update_breakpoints(snapshot);
        let breakpoints: Vec<Breakpoint> = statuses
            .iter()
            .map(|s| self.breakpoint_body(s, None))
            .collect();
        Ok(Some(json!({ "breakpoints": breakpoints })))
    }

    fn set_exception_breakpoints(&mut self, request: &Request) -> Result<Body> {
        let args: SetExceptionBreakpointsArguments = parse_args(request)?;
        let session = self.session_mut()?;
        session.breakpoints.set_exception_filters(&args.filters);
        let snapshot = session.breakpoints.snapshot();
        session.debugger.update_breakpoints(snapshot);
        Ok(None)
    }

    fn breakpoint_locations(&mut self, request: &Request) -> Result<Body> {
        let args: BreakpointLocationsArguments = parse_args(request)?;
        let program = self.program()?;
        let start = self.line_in(args.line);
        let end = args.end_line.map(|l| self.line_in(l)).unwrap_or(start);
        let positions: Vec<(u32, u32)> = match Self::source_target(&args.source)? {
            SourceTarget::File(path) => match &program.debug {
                Some(debug) => {
                    let query = program
                        .source_map
                        .to_dwarf(&path)
                        .unwrap_or_else(|| paths::normalize(&path));
                    let files = debug.lines().find_files(&query);
                    debug.lines().statement_positions(&files, start, end)
                }
                None => Vec::new(),
            },
            SourceTarget::Wat => {
                let disassembly = program.disassembly()?;
                (start..=end)
                    .filter(|l| disassembly.offset_of_line(l.saturating_sub(1)).is_some())
                    .map(|l| (l, 0))
                    .collect()
            }
        };
        let breakpoints: Vec<BreakpointLocation> = positions
            .into_iter()
            .map(|(line, column)| BreakpointLocation {
                line: self.line_out(line),
                column: (column > 0).then(|| self.column_out(column)),
            })
            .collect();
        Ok(Some(json!({ "breakpoints": breakpoints })))
    }

    fn file_source_object(&self, path: &str) -> Source {
        let exists = Path::new(path).exists();
        Source {
            name: Some(paths::file_name(path).to_string()),
            path: Some(native_path(path)),
            source_reference: None,
            presentation_hint: (!exists).then(|| "deemphasize".to_string()),
            origin: (!exists).then(|| "not found on disk; configure `sourceMap`".to_string()),
        }
    }

    fn wat_source_object(&self, program: &ProgramInfo) -> Source {
        Source {
            name: Some(program.wat_name()),
            path: None,
            source_reference: Some(WAT_SOURCE_REFERENCE),
            presentation_hint: None,
            origin: Some("WebAssembly text format (disassembled)".into()),
        }
    }

    fn code_source_object(&self, program: &ProgramInfo, pc: u32) -> Option<Source> {
        match program.code_source(pc)? {
            CodeSource::File { path, .. } => Some(self.file_source_object(&path)),
            CodeSource::Wat { .. } => Some(self.wat_source_object(program)),
        }
    }

    fn stack_trace(&mut self, request: &Request) -> Result<Body> {
        let args: StackTraceArguments = parse_args(request)?;
        let program = self.program()?;
        let trace = self
            .session()?
            .debugger
            .stack_trace(args.start_frame.unwrap_or(0), args.levels)?;
        let frames: Vec<StackFrame> = trace
            .frames
            .iter()
            .map(|frame| {
                let mut stack_frame = StackFrame {
                    id: frame.id,
                    name: frame.name.clone(),
                    instruction_pointer_reference: frame.pc.map(format_code_address),
                    module_id: Some(MODULE_ID),
                    ..StackFrame::default()
                };
                match &frame.source {
                    Some(CodeSource::File { path, line, column }) => {
                        let source = self.file_source_object(path);
                        if source.presentation_hint.is_some() {
                            stack_frame.presentation_hint = Some("subtle".into());
                        }
                        stack_frame.source = Some(source);
                        stack_frame.line = self.line_out(*line);
                        stack_frame.column = self.column_out((*column).max(1));
                    }
                    Some(CodeSource::Wat { line }) => {
                        stack_frame.source = Some(self.wat_source_object(&program));
                        stack_frame.line = self.line_out(*line);
                        stack_frame.column = self.column_out(1);
                    }
                    None => {}
                }
                stack_frame
            })
            .collect();
        Ok(Some(
            json!({ "stackFrames": frames, "totalFrames": trace.total }),
        ))
    }

    fn scopes(&mut self, request: &Request) -> Result<Body> {
        let args: ScopesArguments = parse_args(request)?;
        let scopes: Vec<Scope> = self
            .session()?
            .debugger
            .scopes(args.frame_id)?
            .into_iter()
            .map(|s| Scope {
                name: s.name,
                presentation_hint: s.hint.map(str::to_string),
                variables_reference: s.reference,
                named_variables: s.named,
                indexed_variables: s.indexed,
                expensive: s.expensive,
            })
            .collect();
        Ok(Some(json!({ "scopes": scopes })))
    }

    fn variables(&mut self, request: &Request) -> Result<Body> {
        let args: VariablesArguments = parse_args(request)?;
        let hex = args.format.and_then(|f| f.hex).unwrap_or(false);
        let variables: Vec<Variable> = self
            .session()?
            .debugger
            .variables(args.variables_reference, args.start, args.count, hex)?
            .into_iter()
            .map(to_variable)
            .collect();
        Ok(Some(json!({ "variables": variables })))
    }

    fn set_variable(&mut self, request: &Request) -> Result<Body> {
        let args: SetVariableArguments = parse_args(request)?;
        let hex = args.format.and_then(|f| f.hex).unwrap_or(false);
        let info = self.session()?.debugger.set_variable(
            args.variables_reference,
            &args.name,
            &args.value,
            hex,
        )?;
        Ok(Some(json!({
            "value": info.value,
            "type": info.type_name,
            "variablesReference": info.reference,
            "namedVariables": info.named,
            "indexedVariables": info.indexed,
            "memoryReference": info.memory_reference,
        })))
    }

    fn evaluate(&mut self, request: &Request) -> Result<Body> {
        let args: EvaluateArguments = parse_args(request)?;
        let hex = args.format.and_then(|f| f.hex).unwrap_or(false);
        let context = match args.context.as_deref() {
            Some("watch") => EvalContext::Watch,
            Some("hover") => EvalContext::Hover,
            Some("clipboard") => EvalContext::Clipboard,
            _ => EvalContext::Repl,
        };
        let expression = args.expression.trim();
        if expression.is_empty() {
            return Err(Error::Eval("empty expression".into()));
        }
        let info = self
            .session()?
            .debugger
            .evaluate(expression, args.frame_id, context, hex)?;
        Ok(Some(json!({
            "result": info.value,
            "type": info.type_name,
            "variablesReference": info.reference,
            "namedVariables": info.named,
            "indexedVariables": info.indexed,
            "memoryReference": info.memory_reference,
        })))
    }

    fn resume(&mut self, mode: ResumeMode) -> Result<()> {
        self.session_mut()?.debugger.resume(mode)
    }

    fn step(&mut self, request: &Request, kind: StepKind) -> Result<Body> {
        let args: StepArguments = parse_args(request)?;
        let granularity = match args.granularity.as_deref() {
            Some("instruction") => Granularity::Instruction,
            _ => Granularity::Line,
        };
        self.resume(ResumeMode::Step(kind, granularity))?;
        Ok(None)
    }

    fn read_memory(&mut self, request: &Request) -> Result<Body> {
        let args: ReadMemoryArguments = parse_args(request)?;
        let read = self.session()?.debugger.read_memory(
            &args.memory_reference,
            args.offset.unwrap_or(0),
            args.count.min(1 << 20),
        )?;
        Ok(Some(json!({
            "address": format!("0x{:08x}", read.address),
            "unreadableBytes": read.unreadable,
            "data": base64::engine::general_purpose::STANDARD.encode(&read.data),
        })))
    }

    fn write_memory(&mut self, request: &Request) -> Result<Body> {
        let args: WriteMemoryArguments = parse_args(request)?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(args.data.as_bytes())
            .map_err(|e| Error::Session(format!("invalid base64 data: {e}")))?;
        let written = self.session()?.debugger.write_memory(
            &args.memory_reference,
            args.offset.unwrap_or(0),
            data,
        )?;
        Ok(Some(json!({ "bytesWritten": written })))
    }

    fn disassemble(&mut self, request: &Request) -> Result<Body> {
        let args: DisassembleArguments = parse_args(request)?;
        let program = self.program()?;
        let base = parse_address(&args.memory_reference).ok_or_else(|| {
            Error::Session(format!(
                "invalid memory reference `{}`",
                args.memory_reference
            ))
        })? as i64
            + args.offset.unwrap_or(0);
        let instructions = program.instructions();
        let disassembly = program.disassembly()?;
        let anchor = instructions.partition_point(|(offset, _)| i64::from(*offset) < base) as i64;
        let start = anchor + args.instruction_offset.unwrap_or(0);
        let count = args.instruction_count.min(10_000) as i64;
        let first = instructions.first().map(|(o, _)| *o).unwrap_or(0);
        let last = instructions.last().map(|(o, _)| *o).unwrap_or(0);
        let mut previous_location: Option<(String, u32)> = None;
        let list: Vec<DisassembledInstruction> = (start..start + count)
            .map(|index| {
                if index < 0 || index as usize >= instructions.len() {
                    let address = if index < 0 {
                        u64::from(first).saturating_sub(index.unsigned_abs())
                    } else {
                        u64::from(last) + (index as u64 - instructions.len() as u64) + 1
                    };
                    return DisassembledInstruction {
                        address: format!("0x{address:08x}"),
                        instruction: String::new(),
                        presentation_hint: Some("invalid".into()),
                        ..DisassembledInstruction::default()
                    };
                }
                let (offset, function) = instructions[index as usize];
                let next = instructions
                    .get(index as usize + 1)
                    .map(|(o, _)| *o)
                    .filter(|o| program.module.function_at(*o).map(|f| f.index) == Some(function))
                    .unwrap_or(offset + 1);
                let bytes = &program.module.bytes()
                    [offset as usize..(next as usize).min(offset as usize + 16)];
                let is_first = program
                    .module
                    .function(function)
                    .and_then(|f| f.body.as_ref())
                    .and_then(|b| b.first_instruction())
                    == Some(offset);
                let mut instruction = DisassembledInstruction {
                    address: format_code_address(offset),
                    instruction_bytes: Some(
                        bytes
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    instruction: disassembly
                        .instruction_text(offset)
                        .unwrap_or("?")
                        .to_string(),
                    symbol: is_first.then(|| {
                        program
                            .function_name_at(offset)
                            .unwrap_or_else(|| program.module.display_function_name(function))
                    }),
                    ..DisassembledInstruction::default()
                };
                if let Some((line, path)) = program.dwarf_line(offset) {
                    let key = (path.clone(), line.line);
                    if previous_location.as_ref() != Some(&key) {
                        instruction.location = Some(self.file_source_object(&path));
                        instruction.line = Some(self.line_out(line.line));
                        instruction.column =
                            (line.column > 0).then(|| self.column_out(line.column));
                        previous_location = Some(key);
                    }
                } else if let Some(line) = disassembly.line_of(offset) {
                    instruction.location = Some(self.wat_source_object(&program));
                    instruction.line = Some(self.line_out(line + 1));
                }
                instruction
            })
            .collect();
        Ok(Some(json!({ "instructions": list })))
    }

    fn loaded_sources(&mut self) -> Result<Body> {
        let program = self.program()?;
        let mut sources = Vec::new();
        if let Some(debug) = &program.debug {
            for file in debug.lines().files_with_code() {
                if let Some(path) = debug.lines().file_path(file) {
                    let local = program.source_map.to_local(path);
                    sources.push(self.file_source_object(&local));
                }
            }
        }
        sources.push(self.wat_source_object(&program));
        Ok(Some(json!({ "sources": sources })))
    }

    fn source(&mut self, request: &Request) -> Result<Body> {
        let args: SourceArguments = parse_args(request)?;
        let reference = args
            .source
            .as_ref()
            .and_then(|s| s.source_reference)
            .or(args.source_reference)
            .filter(|r| *r > 0);
        if reference == Some(WAT_SOURCE_REFERENCE) {
            let text = self.program()?.disassembly()?.text();
            return Ok(Some(json!({ "content": text, "mimeType": "text/x-wat" })));
        }
        let path = args
            .source
            .as_ref()
            .and_then(|s| s.path.clone())
            .ok_or_else(|| Error::Session("unknown source".into()))?;
        let program = self.program()?;
        if !is_program_source(&program, &path) {
            return Err(Error::Session(format!(
                "{path} is not a source file of the debugged program"
            )));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| Error::Session(format!("cannot read {path}: {e}")))?;
        Ok(Some(json!({ "content": content })))
    }

    fn modules(&mut self) -> Result<Body> {
        let module = self.module_description()?;
        Ok(Some(json!({ "modules": [module], "totalModules": 1 })))
    }

    fn exception_info(&mut self) -> Result<Body> {
        let details =
            self.session()?.debugger.exception_info()?.ok_or_else(|| {
                Error::Session("the program is not stopped on an exception".into())
            })?;
        Ok(Some(json!({
            "exceptionId": details.id,
            "description": details.description,
            "breakMode": "always",
            "details": {
                "message": details.description,
                "typeName": details.id,
            }
        })))
    }

    fn terminate_session(&mut self) -> Result<()> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        session.debugger.terminate();
        self.send_terminated()
    }

    fn send_terminated(&mut self) -> Result<()> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        if session.terminated_sent {
            return Ok(());
        }
        session.terminated_sent = true;
        self.send_event("terminated", None)
    }

    fn on_target_event(&mut self, event: TargetEvent) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if session.terminated_sent {
                return Ok(());
            }
            session.debugger.observe(&event);
        }
        match event {
            TargetEvent::Stopped(info) => self.send_event(
                "stopped",
                Some(json!({
                    "reason": info.reason.as_dap(),
                    "description": info.description,
                    "text": info.text,
                    "threadId": THREAD_ID,
                    "allThreadsStopped": true,
                    "hitBreakpointIds": info.hit_breakpoint_ids,
                })),
            ),
            TargetEvent::Output { category, text } => self.send_event(
                "output",
                Some(json!({ "category": category.as_dap(), "output": text })),
            ),
            TargetEvent::Exited { code } => {
                self.send_event("exited", Some(json!({ "exitCode": code })))?;
                self.send_terminated()
            }
        }
    }

    fn respond(&mut self, request: &Request, outcome: Result<Body>) -> Result<()> {
        let (success, message, body) = match outcome {
            Ok(body) => (true, None, body.map(strip_nulls)),
            Err(e) => (false, Some(e.to_string()), None),
        };
        self.seq += 1;
        let response = Response {
            seq: self.seq,
            request_seq: request.seq,
            success,
            command: request.command.clone(),
            message,
            body,
        };
        self.writer.write_message(&Message::Response(response))?;
        Ok(())
    }

    fn send_event(&mut self, event: &str, body: Option<Value>) -> Result<()> {
        self.seq += 1;
        let event = Event {
            seq: self.seq,
            event: event.to_string(),
            body: body.map(strip_nulls),
        };
        self.writer.write_message(&Message::Event(event))?;
        Ok(())
    }

    /// State of the debuggee, if a program has been launched.
    pub fn run_state(&self) -> Option<RunState> {
        self.session.as_ref().map(|s| s.debugger.state())
    }
}

impl<R, W> Server<R, W> {
    fn check_token(&mut self, request: &Request) -> bool {
        let Some(expected) = &self.required_token else {
            return true;
        };
        let provided = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("serverToken"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.authenticated = constant_time_eq(provided.as_bytes(), expected.as_bytes());
        self.authenticated
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Only the program and the sources named in its DWARF may be read by clients.
fn is_program_source(program: &ProgramInfo, path: &str) -> bool {
    let wanted = paths::key(path);
    if paths::key(&program.path.to_string_lossy()) == wanted {
        return true;
    }
    program.debug.as_ref().is_some_and(|debug| {
        debug.lines().files().iter().any(|f| {
            paths::key(&program.source_map.to_local(f)) == wanted || paths::key(f) == wanted
        })
    })
}

fn parse_args<T: serde::de::DeserializeOwned + Default>(request: &Request) -> Result<T> {
    match &request.arguments {
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| {
            Error::Session(format!("invalid arguments for `{}`: {e}", request.command))
        }),
        None => Ok(T::default()),
    }
}

fn to_variable(info: VariableInfo) -> Variable {
    Variable {
        presentation_hint: info.read_only.then(|| VariablePresentationHint {
            kind: None,
            attributes: vec!["readOnly".into()],
        }),
        name: info.name,
        value: info.value,
        type_name: info.type_name,
        evaluate_name: info.evaluate_name,
        variables_reference: info.reference,
        named_variables: info.named,
        indexed_variables: info.indexed,
        memory_reference: info.memory_reference,
    }
}

/// Uses the host's path separators for drive-letter paths on Windows.
fn native_path(path: &str) -> String {
    if cfg!(windows) && paths::is_absolute(path) && path.as_bytes().get(1) == Some(&b':') {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        other => other,
    }
}
