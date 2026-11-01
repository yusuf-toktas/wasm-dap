//! Debug Adapter Protocol sessions against the in-process server, driven the
//! way an editor drives them: request, wait for response or event, repeat.

use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use wasm_dap_core::server::Server;
use wasm_dap_testkit::fixtures::{fixtures_dir, source_path};
use wasm_dap_testkit::{
    DapClient, RustTarget, find_var, line_of, rust_fixture, wat_module, wat_source,
};

fn start() -> DapClient {
    let (client_reader, server_writer) = std::io::pipe().expect("pipe");
    let (server_reader, client_writer) = std::io::pipe().expect("pipe");
    std::thread::spawn(move || {
        let mut server = Server::new(BufReader::new(server_reader), server_writer);
        server.run().expect("server run");
    });
    DapClient::from_streams(client_reader, client_writer)
}

fn arith() -> PathBuf {
    rust_fixture("arith.rs", RustTarget::Library)
}

fn arith_source() -> String {
    source_path("arith.rs").to_string_lossy().into_owned()
}

fn source_map() -> Value {
    json!({ "/fixtures": fixtures_dir().to_string_lossy() })
}

fn launch_arith(client: &mut DapClient, invoke: &str, args: Value) {
    client.launch(json!({
        "program": arith(),
        "invoke": invoke,
        "invokeArgs": args,
        "sourceMap": source_map(),
    }));
}

fn break_at(client: &mut DapClient, marker: &str) -> u32 {
    let line = line_of("arith.rs", marker);
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": arith_source() }, "breakpoints": [{ "line": line }] }),
    );
    let bp = &body["breakpoints"][0];
    assert_eq!(bp["verified"], true, "{bp}");
    assert_eq!(bp["line"], line);
    line
}

fn evaluate(client: &mut DapClient, frame_id: &Value, expression: &str) -> String {
    let body = client.ok(
        "evaluate",
        json!({ "expression": expression, "frameId": frame_id, "context": "watch" }),
    );
    body["result"].as_str().unwrap().to_string()
}

fn finish(client: &mut DapClient) -> i64 {
    let exited = client.event("exited");
    client.event("terminated");
    let code = exited.body.unwrap()["exitCode"].as_i64().unwrap();
    client.disconnect();
    code
}

#[test]
fn initialize_announces_capabilities() {
    let mut client = start();
    let caps = client.ok("initialize", json!({ "adapterID": "wasm-dap" }));
    for key in [
        "supportsConfigurationDoneRequest",
        "supportsFunctionBreakpoints",
        "supportsConditionalBreakpoints",
        "supportsHitConditionalBreakpoints",
        "supportsLogPoints",
        "supportsDisassembleRequest",
        "supportsInstructionBreakpoints",
        "supportsSteppingGranularity",
        "supportsReadMemoryRequest",
        "supportsWriteMemoryRequest",
        "supportsExceptionInfoRequest",
    ] {
        assert_eq!(caps[key], true, "{key} missing in {caps}");
    }
    let filters = caps["exceptionBreakpointFilters"].as_array().unwrap();
    assert_eq!(filters.len(), 2);
    assert!(filters.iter().all(|f| f["default"] == true));
    client.disconnect();
}

#[test]
fn launch_errors_are_explained() {
    let mut client = start();
    client.ok("initialize", json!({}));
    let message = client.fails("launch", json!({}));
    assert!(message.contains("program"), "{message}");
    let message = client.fails("launch", json!({ "program": "does/not/exist.wasm" }));
    assert!(message.contains("does not exist"), "{message}");
    let message = client.fails("launch", json!({ "program": arith() }));
    assert!(
        message.contains("invoke") && message.contains("fib"),
        "{message}"
    );
    let message = client.fails("launch", json!({ "program": arith(), "invoke": "nope" }));
    assert!(
        message.contains("nope") && message.contains("sum_points"),
        "{message}"
    );
    let message = client.fails(
        "launch",
        json!({ "program": arith(), "invoke": "fib", "invokeArgs": [] }),
    );
    assert!(message.contains("expects 1 argument"), "{message}");
    client.disconnect();
}

#[test]
fn source_breakpoints_stop_with_real_frames_and_variables() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([5]));
    let line = break_at(&mut client, "BP: fib-sum");
    client.ok("configurationDone", json!({}));

    let stopped = client.stopped();
    assert_eq!(stopped["reason"], "breakpoint");
    assert_eq!(stopped["hitBreakpointIds"].as_array().unwrap().len(), 1);

    let trace = client.ok("stackTrace", json!({ "threadId": 1 }));
    let frames = trace["stackFrames"].as_array().unwrap();
    assert!(frames.len() >= 4, "recursion depth: {trace}");
    let top = &frames[0];
    assert_eq!(top["name"], "arith::fib");
    assert_eq!(top["line"], line);
    assert!(
        top["source"]["path"]
            .as_str()
            .unwrap()
            .ends_with("arith.rs")
    );
    assert!(
        top["instructionPointerReference"]
            .as_str()
            .unwrap()
            .starts_with("0x")
    );
    let frame_id = top["id"].clone();

    let locals = client.scope_variables(frame_id.as_i64().unwrap(), "Locals");
    assert_eq!(find_var(&locals, "n")["value"], "2");
    assert_eq!(find_var(&locals, "a")["value"], "1");
    assert_eq!(find_var(&locals, "b")["value"], "0");
    assert_eq!(find_var(&locals, "n")["type"], "u32");
    assert_eq!(evaluate(&mut client, &frame_id, "a + b"), "1");
    let deepest = ["n"; 100].join("+");
    assert_eq!(evaluate(&mut client, &frame_id, &deepest), "200");
    let too_deep = ["n"; 150].join("+");
    let refused = client.fails(
        "evaluate",
        json!({ "expression": too_deep, "frameId": frame_id, "context": "watch" }),
    );
    assert!(refused.contains("nested too deeply"), "{refused}");

    let caller = frames[1]["id"].clone();
    assert_eq!(evaluate(&mut client, &caller, "n"), "3");

    let wasm_locals = client.scope_variables(frame_id.as_i64().unwrap(), "Wasm Locals");
    assert!(
        wasm_locals
            .iter()
            .any(|v| v["name"] == "$param0" && v["value"] == "2")
    );

    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(evaluate(&mut client, &top["id"], "n"), "3");

    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": arith_source() }, "breakpoints": [] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn result_of_invoked_function_is_printed() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([10]));
    client.ok("configurationDone", json!({}));
    let output = client.wait_output("fib(10) = 55");
    assert!(output.contains("fib(10) = 55"));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn step_in_out_and_over_follow_source_lines() {
    let mut client = start();
    launch_arith(&mut client, "shapes", json!([]));
    let line = break_at(&mut client, "BP: shapes-total");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");

    client.ok("stepIn", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "step");
    let inner = client.top_frame();
    assert_eq!(inner["name"], "arith::area", "{inner}");

    client.ok("stepOut", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "step");
    let back = client.top_frame();
    assert_eq!(back["name"], "arith::shapes");
    assert_eq!(back["line"], line);

    client.ok("next", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "step");
    let after = client.top_frame();
    assert_eq!(after["name"], "arith::shapes");
    assert!(after["line"].as_u64().unwrap() > u64::from(line), "{after}");
    assert_eq!(evaluate(&mut client, &after["id"], "total"), "24");

    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn function_breakpoints_honor_conditions_and_hit_counts() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([6]));
    let body = client.ok(
        "setFunctionBreakpoints",
        json!({ "breakpoints": [{ "name": "fib", "condition": "n == 1" }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "function breakpoint");
    let top = client.top_frame();
    assert_eq!(top["name"], "arith::fib");
    assert_eq!(evaluate(&mut client, &top["id"], "n"), "1");

    client.ok(
        "setFunctionBreakpoints",
        json!({ "breakpoints": [{ "name": "fib", "hitCondition": "== 3" }] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "function breakpoint");

    let missing = client.ok(
        "setFunctionBreakpoints",
        json!({ "breakpoints": [{ "name": "does_not_exist" }] }),
    );
    assert_eq!(missing["breakpoints"][0]["verified"], false);
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn logpoints_print_without_stopping() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([3]));
    let line = line_of("arith.rs", "BP: fib-base");
    client.ok(
        "setBreakpoints",
        json!({
            "source": { "path": arith_source() },
            "breakpoints": [{ "line": line, "logMessage": "base case n={n} {{literal}}" }]
        }),
    );
    client.ok("configurationDone", json!({}));
    let output = client.wait_output("fib(3) = 2");
    assert!(output.contains("base case n=1 {literal}"), "{output}");
    assert!(output.contains("base case n=0"), "{output}");
    assert!(!client.has_event_within("stopped", Duration::from_millis(100)));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn traps_pause_with_exception_details() {
    let mut client = start();
    launch_arith(&mut client, "divide", json!([7, 0]));
    client.ok("configurationDone", json!({}));
    let stopped = client.stopped();
    assert_eq!(stopped["reason"], "exception");
    assert!(
        stopped["description"]
            .as_str()
            .unwrap()
            .contains("unreachable"),
        "{stopped}"
    );
    let info = client.ok("exceptionInfo", json!({ "threadId": 1 }));
    assert_eq!(info["breakMode"], "always");
    let trace = client.ok("stackTrace", json!({ "threadId": 1 }));
    let names: Vec<&str> = trace["stackFrames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"arith::divide"), "{names:?}");
    client.ok("continue", json!({ "threadId": 1 }));
    let output = client.wait_output("error:");
    assert!(output.contains("unreachable"), "{output}");
    assert_eq!(finish(&mut client), 1);
}

#[test]
fn trap_filter_can_be_disabled() {
    let mut client = start();
    launch_arith(&mut client, "divide", json!([1, 0]));
    client.ok("setExceptionBreakpoints", json!({ "filters": [] }));
    client.ok("configurationDone", json!({}));
    assert_eq!(finish(&mut client), 1);
}

#[test]
fn pause_interrupts_a_running_loop_and_breakpoints_apply_live() {
    let mut client = start();
    launch_arith(&mut client, "spin", json!([0]));
    client.ok("configurationDone", json!({}));
    std::thread::sleep(Duration::from_millis(200));
    client.ok("pause", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "pause");
    let top = client.top_frame();
    assert_eq!(top["name"], "arith::spin");
    let i: u64 = evaluate(&mut client, &top["id"], "i").parse().unwrap();
    assert!(i > 0);

    client.ok("continue", json!({ "threadId": 1 }));
    std::thread::sleep(Duration::from_millis(50));
    let line = break_at(&mut client, "BP: spin");
    assert_eq!(client.stopped()["reason"], "breakpoint");
    assert_eq!(client.top_frame()["line"], line);

    // The breakpoint sits on the first line of the loop body, so it must hit
    // on every iteration, not only when the loop is entered.
    let top = client.top_frame();
    let first: u64 = evaluate(&mut client, &top["id"], "i").parse().unwrap();
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    let second: u64 = evaluate(&mut client, &top["id"], "i").parse().unwrap();
    assert_eq!(second, first + 1);

    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": arith_source() }, "breakpoints": [] }),
    );
    client.ok("next", json!({ "threadId": 1 }));
    client.stopped();
    assert_eq!(client.top_frame()["line"], line + 1);
    client.ok("next", json!({ "threadId": 1 }));
    client.stopped();
    assert_eq!(
        client.top_frame()["line"],
        line,
        "stepping wraps around the loop"
    );
    client.disconnect();
}

#[test]
fn memory_and_variables_can_be_modified() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([5]));
    break_at(&mut client, "BP: fib-sum");
    client.ok("configurationDone", json!({}));
    client.stopped();
    let top = client.top_frame();
    let frame_id = top["id"].clone();

    let address = client.ok(
        "evaluate",
        json!({ "expression": "&a", "frameId": frame_id }),
    );
    let reference = address["memoryReference"].as_str().unwrap().to_string();
    let read = client.ok(
        "readMemory",
        json!({ "memoryReference": reference, "count": 4 }),
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(read["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(u32::from_le_bytes(bytes.try_into().unwrap()), 1);

    let data = base64::engine::general_purpose::STANDARD.encode(40u32.to_le_bytes());
    let written = client.ok(
        "writeMemory",
        json!({ "memoryReference": reference, "data": data }),
    );
    assert_eq!(written["bytesWritten"], 4);
    assert_eq!(evaluate(&mut client, &frame_id, "a"), "40");

    let scopes = client.ok("scopes", json!({ "frameId": frame_id }));
    let locals = scopes["scopes"][0]["variablesReference"].clone();
    let set = client.ok(
        "setVariable",
        json!({ "variablesReference": locals, "name": "b", "value": "2" }),
    );
    assert_eq!(set["value"], "2");
    assert_eq!(evaluate(&mut client, &frame_id, "a + b"), "42");

    let wasm_locals = scopes["scopes"][1]["variablesReference"].clone();
    let refused = client.fails(
        "setVariable",
        json!({ "variablesReference": wasm_locals, "name": "$param0", "value": "1" }),
    );
    assert!(refused.contains("read-only"), "{refused}");
    client.disconnect();
}

#[test]
fn disassembly_and_instruction_stepping() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([3]));
    break_at(&mut client, "BP: fib-sum");
    client.ok("configurationDone", json!({}));
    client.stopped();
    let top = client.top_frame();
    let ip = top["instructionPointerReference"]
        .as_str()
        .unwrap()
        .to_string();

    let body = client.ok(
        "disassemble",
        json!({ "memoryReference": ip, "instructionOffset": -3, "instructionCount": 8, "resolveSymbols": true }),
    );
    let instructions = body["instructions"].as_array().unwrap();
    assert_eq!(instructions.len(), 8);
    assert_eq!(instructions[3]["address"], ip.as_str());
    assert!(!instructions[3]["instruction"].as_str().unwrap().is_empty());
    let next_address = instructions[4]["address"].as_str().unwrap().to_string();

    client.ok(
        "next",
        json!({ "threadId": 1, "granularity": "instruction" }),
    );
    assert_eq!(client.stopped()["reason"], "step");
    assert_eq!(
        client.top_frame()["instructionPointerReference"],
        next_address.as_str()
    );

    let bp = client.ok(
        "setInstructionBreakpoints",
        json!({ "breakpoints": [{ "instructionReference": instructions[6]["address"] }] }),
    );
    assert_eq!(bp["breakpoints"][0]["verified"], true);
    client.ok("continue", json!({ "threadId": 1 }));
    let stopped = client.stopped();
    assert_eq!(stopped["reason"], "instruction breakpoint");
    assert_eq!(
        client.top_frame()["instructionPointerReference"],
        instructions[6]["address"]
    );
    client.disconnect();
}

const ADD_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (global $calls (export "calls") (mut i32) (i32.const 0))
  (func $add (export "add") (param $a i32) (param $b i32) (result i32)
    global.get $calls
    i32.const 1
    i32.add
    global.set $calls
    local.get $a
    local.get $b
    i32.add)
  (func $twice (export "twice") (param $x i32) (result i32)
    local.get $x
    local.get $x
    call $add))
"#;

#[test]
fn modules_without_dwarf_are_debugged_in_text_format() {
    let path = wat_module("add-nodwarf", ADD_WAT);
    let mut client = start();
    client.launch(json!({ "program": path, "invoke": "twice", "invokeArgs": [21] }));
    let body = client.ok(
        "setFunctionBreakpoints",
        json!({ "breakpoints": [{ "name": "add" }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "function breakpoint");

    let top = client.top_frame();
    assert_eq!(top["name"], "add");
    let source = &top["source"];
    assert_eq!(source["sourceReference"], 1);
    assert!(source["name"].as_str().unwrap().ends_with(".wat"));
    let text = client.ok("source", json!({ "sourceReference": 1, "source": source }));
    let content = text["content"].as_str().unwrap();
    let lines: Vec<&str> = content.lines().collect();
    let shown = lines[top["line"].as_u64().unwrap() as usize - 1];
    assert!(shown.contains("global.get"), "line {shown:?}");

    let locals = client.scope_variables(top["id"].as_i64().unwrap(), "Wasm Locals");
    assert_eq!(find_var(&locals, "$a")["value"], "21");
    assert_eq!(find_var(&locals, "$b")["value"], "21");
    let globals = client.scope_variables(top["id"].as_i64().unwrap(), "Globals");
    assert_eq!(find_var(&globals, "calls")["value"], "0");
    let scopes = client.ok("scopes", json!({ "frameId": top["id"] }));
    let globals_ref = scopes["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Globals")
        .unwrap()["variablesReference"]
        .clone();
    let set = client.ok(
        "setVariable",
        json!({ "variablesReference": globals_ref, "name": "calls", "value": "41" }),
    );
    assert_eq!(set["value"], "41");
    let invalid = client.fails(
        "setVariable",
        json!({ "variablesReference": globals_ref, "name": "calls", "value": "many" }),
    );
    assert!(invalid.contains("not a valid"), "{invalid}");

    let target_line = lines
        .iter()
        .position(|l| l.trim() == "local.get $b")
        .unwrap() as u32
        + 1;
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": source, "breakpoints": [{ "line": target_line }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true);
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    assert_eq!(client.top_frame()["line"], target_line);
    let frame = client.top_frame()["id"].as_i64().unwrap();
    let globals = client.scope_variables(frame, "Globals");
    assert_eq!(find_var(&globals, "calls")["value"], "42");
    let stack = client.scope_variables(frame, "Operand Stack");
    assert_eq!(stack.len(), 1);
    assert_eq!(stack[0]["value"], "21");

    client.ok(
        "setBreakpoints",
        json!({ "source": source, "breakpoints": [] }),
    );
    client.ok("setFunctionBreakpoints", json!({ "breakpoints": [] }));
    client.ok("continue", json!({ "threadId": 1 }));
    let output = client.wait_output("twice(21) = 42");
    assert!(output.contains("twice(21) = 42"));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn wat_files_are_debugged_at_the_source_level() {
    let path = wat_source("add-source", ADD_WAT);
    let line = ADD_WAT
        .lines()
        .position(|l| l.trim() == "local.get $b")
        .unwrap() as u32
        + 1;
    let mut client = start();
    client.launch(json!({ "program": path, "invoke": "add", "invokeArgs": [2, 3] }));
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": path }, "breakpoints": [{ "line": line }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(top["line"], line);
    let shown = top["source"]["path"].as_str().unwrap().to_lowercase();
    assert!(shown.ends_with(".wat"), "{shown}");
    client.ok("next", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "step");
    assert_eq!(client.top_frame()["line"], line + 1);
    client.ok("continue", json!({ "threadId": 1 }));
    assert!(client.wait_output("add(2, 3) = 5").contains("= 5"));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn unresolved_imports_stop_as_host_errors() {
    let path = wat_module(
        "missing-import",
        r#"(module
            (import "env" "log_value" (func $log (param i32)))
            (func (export "run") (result i32)
                i32.const 7
                call $log
                i32.const 1))"#,
    );
    let mut client = start();
    client.launch(json!({ "program": path, "invoke": "run" }));
    client.ok("configurationDone", json!({}));
    let stopped = client.stopped();
    assert_eq!(stopped["reason"], "exception");
    assert!(
        stopped["text"]
            .as_str()
            .unwrap()
            .contains("unresolved import `env::log_value`"),
        "{stopped}"
    );
    let info = client.ok("exceptionInfo", json!({ "threadId": 1 }));
    assert_eq!(info["exceptionId"], "UnresolvedImport");
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 1);
}

#[test]
fn stop_on_entry_pauses_before_the_first_instruction() {
    let path = wat_module("entry", ADD_WAT);
    let mut client = start();
    client.launch(
        json!({ "program": path, "invoke": "add", "invokeArgs": [1, 2], "stopOnEntry": true }),
    );
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "entry");
    assert_eq!(client.top_frame()["name"], "add");
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn wasi_programs_get_arguments_environment_output_and_exit_codes() {
    let program = rust_fixture("wasi_app.rs", RustTarget::Wasi);
    let mut client = start();
    client.launch(json!({
        "program": program,
        "args": ["--fail"],
        "env": { "GREETING_NAME": "dap" },
        "sourceMap": source_map(),
    }));
    client.ok("configurationDone", json!({}));
    let output = client.wait_output("failing on request");
    assert!(output.contains("hello, dap!"), "{output}");
    assert!(output.contains("args=2 count=5 total=153"), "{output}");
    assert_eq!(finish(&mut client), 3);
}

#[test]
fn std_rust_values_are_inspectable() {
    let program = rust_fixture("wasi_app.rs", RustTarget::Wasi);
    let source = source_path("wasi_app.rs").to_string_lossy().into_owned();
    let line = line_of("wasi_app.rs", "BP: loop");
    let mut client = start();
    client.launch(json!({ "program": program, "args": ["one"], "sourceMap": source_map() }));
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source }, "breakpoints": [{ "line": line, "condition": "i == 4" }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(top["line"], line);
    let id = top["id"].clone();
    assert_eq!(evaluate(&mut client, &id, "i"), "4");
    assert_eq!(evaluate(&mut client, &id, "f"), "24");
    assert_eq!(evaluate(&mut client, &id, "stats.count"), "4");
    assert_eq!(evaluate(&mut client, &id, "stats.total"), "9");
    assert_eq!(evaluate(&mut client, &id, "name"), "\"world\"");
    let args = client.ok("evaluate", json!({ "expression": "args", "frameId": id }));
    assert_eq!(args["indexedVariables"], 2, "{args}");
    let children = client.ok(
        "variables",
        json!({ "variablesReference": args["variablesReference"] }),
    );
    assert_eq!(children["variables"][1]["value"], "\"one\"");
    assert_eq!(evaluate(&mut client, &id, "args[1]"), "\"one\"");
    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn sources_modules_and_breakpoint_locations_are_listed() {
    let mut client = start();
    launch_arith(&mut client, "fib", json!([1]));
    let sources = client.ok("loadedSources", json!({}));
    let list = sources["sources"].as_array().unwrap();
    assert!(
        list.iter()
            .any(|s| s["path"].as_str().is_some_and(|p| p.ends_with("arith.rs")))
    );
    assert!(list.iter().any(|s| s["sourceReference"] == 1));

    let content = client.ok("source", json!({ "source": { "path": arith_source() } }));
    assert!(
        content["content"]
            .as_str()
            .unwrap()
            .contains("pub extern \"C\" fn fib")
    );
    let unrelated = fixtures_dir().join("..").join("Cargo.toml");
    let refused = client.fails("source", json!({ "source": { "path": unrelated } }));
    assert!(refused.contains("not a source file"), "{refused}");

    let modules = client.ok("modules", json!({}));
    assert!(
        modules["modules"][0]["symbolStatus"]
            .as_str()
            .unwrap()
            .contains("DWARF")
    );

    let first = line_of("arith.rs", "pub extern \"C\" fn fib");
    let locations = client.ok(
        "breakpointLocations",
        json!({ "source": { "path": arith_source() }, "line": first, "endLine": first + 7 }),
    );
    let lines: Vec<u64> = locations["breakpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["line"].as_u64().unwrap())
        .collect();
    assert!(
        lines.contains(&u64::from(line_of("arith.rs", "BP: fib-sum"))),
        "{lines:?}"
    );
    client.ok("configurationDone", json!({}));
    assert_eq!(finish(&mut client), 0);
}

#[test]
fn requests_out_of_order_fail_gracefully() {
    let mut client = start();
    client.ok("initialize", json!({}));
    assert!(
        client
            .fails("stackTrace", json!({ "threadId": 1 }))
            .contains("launched")
    );
    assert!(
        client
            .fails("bogusRequest", json!({}))
            .contains("unsupported")
    );
    launch_arith(&mut client, "spin", json!([0]));
    assert!(
        client
            .fails("stackTrace", json!({ "threadId": 1 }))
            .contains("not started")
    );
    client.ok("configurationDone", json!({}));
    assert!(
        client
            .fails("scopes", json!({ "frameId": 1 }))
            .contains("running")
    );
    client.ok("terminate", json!({}));
    client.event("terminated");
    client.disconnect();
}

#[test]
fn dwarf5_programs_are_debugged_like_dwarf4() {
    let program = wasm_dap_testkit::rust_fixture_with(
        "arith.rs",
        RustTarget::Library,
        0,
        &["-C", "dwarf-version=5"],
    );
    let mut client = start();
    client.launch(json!({
        "program": program,
        "invoke": "sum_points",
        "invokeArgs": [3],
        "sourceMap": source_map(),
    }));
    let line = break_at(&mut client, "BP: sum-loop");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(top["name"], "arith::sum_points");
    assert_eq!(top["line"], line);
    assert_eq!(evaluate(&mut client, &top["id"], "points[1].y"), "4");
    assert_eq!(evaluate(&mut client, &top["id"], "p.x * p.y"), "2");
    client.ok("next", json!({ "threadId": 1 }));
    client.stopped();
    assert_eq!(client.top_frame()["line"], line + 1);
    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": arith_source() }, "breakpoints": [] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    assert!(client.wait_output("sum_points(3) = 44").contains("= 44"));
    assert_eq!(finish(&mut client), 0);
}
