//! Drives the debugging engine directly against real compiled modules.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::json;
use wasm_dap_core::debugger::{
    BreakpointManager, CodeSource, Debugger, EntryPoint, EvalContext, Granularity, LaunchOptions,
    ResumeMode, SourceBreakpointRequest, SourceTarget, StepKind, StopReason, TargetEvent,
};
use wasm_dap_core::dwarf::SourceMap;
use wasm_dap_core::runtime::{Program, WasiOptions};
use wasm_dap_testkit::{RustTarget, line_of, rust_fixture, source_map};

struct Harness {
    debugger: Debugger,
    events: Receiver<TargetEvent>,
    breakpoints: BreakpointManager,
    output: String,
}

impl Harness {
    fn launch(entry: EntryPoint) -> Self {
        let path = rust_fixture("arith.rs", RustTarget::Library);
        let program = Program::load(&path).expect("load program");
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let options = LaunchOptions {
            entry,
            wasi: WasiOptions::default(),
            stop_on_entry: false,
            no_debug: false,
        };
        let debugger = Debugger::launch(program, options, SourceMap::new([source_map()]), sink)
            .expect("launch");
        Self {
            debugger,
            events: rx,
            breakpoints: BreakpointManager::new(),
            output: String::new(),
        }
    }

    fn set_line_breakpoint(&mut self, marker: &str) -> u32 {
        let line = line_of("arith.rs", marker);
        let path = wasm_dap_testkit::fixtures::source_path("arith.rs");
        let statuses = self.breakpoints.set_source(
            self.debugger.program(),
            &SourceTarget::File(path.to_string_lossy().into_owned()),
            &[SourceBreakpointRequest {
                line,
                ..Default::default()
            }],
        );
        assert!(
            statuses[0].verified,
            "breakpoint not verified: {:?}",
            statuses[0]
        );
        assert_eq!(statuses[0].line, Some(line));
        line
    }

    fn next_event(&mut self) -> TargetEvent {
        loop {
            let event = self
                .events
                .recv_timeout(Duration::from_secs(60))
                .expect("debuggee event");
            self.debugger.observe(&event);
            match event {
                TargetEvent::Output { text, .. } => self.output.push_str(&text),
                other => return other,
            }
        }
    }

    fn expect_stop(&mut self, reason: StopReason) {
        match self.next_event() {
            TargetEvent::Stopped(info) => assert_eq!(info.reason, reason, "{info:?}"),
            other => panic!("expected stop, got {other:?}; output: {}", self.output),
        }
    }

    fn top_line(&self) -> u32 {
        let trace = self.debugger.stack_trace(0, None).unwrap();
        match &trace.frames[0].source {
            Some(CodeSource::File { line, .. }) => *line,
            other => panic!("expected a file location, got {other:?}"),
        }
    }

    fn value_of(&self, expression: &str) -> String {
        self.debugger
            .evaluate(expression, Some(1), EvalContext::Watch, false)
            .unwrap_or_else(|e| panic!("evaluating {expression}: {e}"))
            .value
    }
}

#[test]
fn breakpoint_stops_inside_recursion_with_real_state() {
    let mut h = Harness::launch(EntryPoint::Function {
        name: "fib".into(),
        args: vec![json!(5)],
    });
    let line = h.set_line_breakpoint("BP: fib-sum");
    h.debugger.start(h.breakpoints.snapshot()).unwrap();
    h.expect_stop(StopReason::Breakpoint);

    let trace = h.debugger.stack_trace(0, None).unwrap();
    assert_eq!(trace.frames[0].name, "arith::fib");
    assert_eq!(h.top_line(), line);
    assert!(
        trace.total >= 4,
        "expected recursive frames, got {}",
        trace.total
    );
    assert!(trace.frames.iter().all(|f| f.name == "arith::fib"));

    // Deepest frame that reaches `a + b` computes fib(2) = fib(1) + fib(0).
    assert_eq!(h.value_of("n"), "2");
    assert_eq!(h.value_of("a"), "1");
    assert_eq!(h.value_of("b"), "0");
    assert_eq!(h.value_of("a + b * 10 == 1"), "true");
    assert_eq!(h.value_of("fib(10)"), "55");

    let scopes = h.debugger.scopes(1).unwrap();
    let names: Vec<&str> = scopes.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names[0], "Locals");
    assert!(names.contains(&"Wasm Locals"));
    assert!(names.contains(&"Globals"));
    let locals = h
        .debugger
        .variables(scopes[0].reference, None, None, false)
        .unwrap();
    let n = locals.iter().find(|v| v.name == "n").unwrap();
    assert_eq!(n.value, "2");
    assert_eq!(n.type_name.as_deref(), Some("u32"));

    h.debugger.resume(ResumeMode::Continue).unwrap();
    h.expect_stop(StopReason::Breakpoint);
    assert_eq!(h.value_of("n"), "3");

    h.breakpoints = BreakpointManager::new();
    h.debugger.update_breakpoints(h.breakpoints.snapshot());
    h.debugger.resume(ResumeMode::Continue).unwrap();
    match h.next_event() {
        TargetEvent::Exited { code } => assert_eq!(code, 0),
        other => panic!("expected exit, got {other:?}"),
    }
    assert!(h.output.contains("fib(5) = 5"), "output: {}", h.output);
}

#[test]
fn line_stepping_moves_between_statements() {
    let mut h = Harness::launch(EntryPoint::Function {
        name: "sum_points".into(),
        args: vec![json!(3)],
    });
    let line = h.set_line_breakpoint("BP: sum-loop");
    h.debugger.start(h.breakpoints.snapshot()).unwrap();
    h.expect_stop(StopReason::Breakpoint);
    assert_eq!(h.top_line(), line);
    assert_eq!(h.value_of("total"), "0");
    assert_eq!(h.value_of("p.x"), "1");
    assert_eq!(h.value_of("points[2].y"), "6");

    h.debugger
        .resume(ResumeMode::Step(StepKind::Over, Granularity::Line))
        .unwrap();
    h.expect_stop(StopReason::Step);
    assert_eq!(h.top_line(), line + 1, "step over moves to `i += 1`");
    assert_eq!(h.value_of("total"), "2");

    h.breakpoints = BreakpointManager::new();
    h.debugger.update_breakpoints(h.breakpoints.snapshot());
    h.debugger.resume(ResumeMode::Continue).unwrap();
    match h.next_event() {
        TargetEvent::Exited { code } => assert_eq!(code, 0),
        other => panic!("expected exit, got {other:?}"),
    }
    assert!(
        h.output.contains("sum_points(3) = 44"),
        "output: {}",
        h.output
    );
}

#[test]
fn rust_values_are_rendered_from_dwarf() {
    let mut h = Harness::launch(EntryPoint::Function {
        name: "shapes".into(),
        args: vec![],
    });
    h.set_line_breakpoint("BP: shapes-total");
    h.debugger.start(h.breakpoints.snapshot()).unwrap();
    h.expect_stop(StopReason::Breakpoint);
    assert_eq!(h.value_of("circle"), "Circle(2)");
    assert_eq!(h.value_of("rect"), "Rect { w: 3, h: 4 }");
    assert_eq!(h.value_of("empty"), "Empty");
    assert_eq!(h.value_of("flag"), "true");
    assert_eq!(h.value_of("ratio"), "0.5");
    assert_eq!(h.value_of("letter"), "'w'");
    assert_eq!(h.value_of("text"), "\"hello wasm\"");
    assert_eq!(h.value_of("maybe"), "Some(7)");
    assert_eq!(h.value_of("rect.w * rect.h"), "12");
}

#[test]
fn statics_globals_and_memory_builtins_are_evaluated() {
    let mut h = Harness::launch(EntryPoint::Function {
        name: "bump".into(),
        args: vec![],
    });
    h.set_line_breakpoint("BP: bump");
    h.debugger.start(h.breakpoints.snapshot()).unwrap();
    h.expect_stop(StopReason::Breakpoint);
    assert_eq!(h.value_of("COUNTER"), "0");
    assert_eq!(h.value_of("arith::COUNTER"), "0");
    assert_eq!(h.value_of("GREETING"), "\"hello wasm\"");
    assert_eq!(h.value_of("GREETING.len()"), "10");

    let address = h
        .debugger
        .evaluate("&COUNTER", Some(1), EvalContext::Watch, false)
        .unwrap();
    let reference = address
        .memory_reference
        .expect("address has a memory reference");
    assert_eq!(h.value_of(&format!("$u32({reference})")), "0");
    let text_ptr = h.value_of("GREETING.data_ptr");
    assert_eq!(h.value_of(&format!("$str({text_ptr}, 5)")), "\"hello\"");

    let stack_pointer: i64 = h.value_of("__stack_pointer").parse().unwrap();
    assert!(stack_pointer > 0);
    assert_eq!(h.value_of("$global0"), stack_pointer.to_string());

    h.debugger
        .resume(ResumeMode::Step(StepKind::Over, Granularity::Line))
        .unwrap();
    h.expect_stop(StopReason::Step);
    assert_eq!(h.value_of("COUNTER"), "1");
}
