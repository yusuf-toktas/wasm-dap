//! End-to-end tests of the `wasm-dap` binary.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wasm_dap_testkit::fixtures::{fixtures_dir, source_path};
use wasm_dap_testkit::{DapClient, RustTarget, line_of, rust_fixture};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wasm-dap"))
}

fn arith() -> PathBuf {
    rust_fixture("arith.rs", RustTarget::Library)
}

fn run(args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("failed to run wasm-dap")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn debug_session_over_stdio() {
    let mut client = DapClient::spawn(&binary());
    client.launch(json!({
        "program": arith(),
        "invoke": "fib",
        "invokeArgs": [4],
        "sourceMap": { "/fixtures": fixtures_dir() },
    }));
    let line = line_of("arith.rs", "BP: fib-sum");
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source_path("arith.rs") }, "breakpoints": [{ "line": line }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true);
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(top["name"], "arith::fib");
    assert_eq!(top["line"], line);
    let result = client.ok(
        "evaluate",
        json!({ "expression": "n * 10", "frameId": top["id"] }),
    );
    assert_eq!(result["result"], "20");
    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source_path("arith.rs") }, "breakpoints": [] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    assert!(client.wait_output("fib(4) = 3").contains("= 3"));
    assert_eq!(client.event("exited").body.unwrap()["exitCode"], 0);
    client.event("terminated");
    client.disconnect();
}

#[test]
fn debug_session_over_tcp() {
    // The server picks the port itself. Probing for a free port here and passing it
    // on is racy: a process spawned at that moment by another test inherits the probe
    // socket until it execs and can accept the connection meant for the server.
    let mut server = Command::new(binary())
        .args(["serve", "--port", "0"])
        .env("WASM_DAP_TOKEN", "e2e-secret")
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let mut stderr = BufReader::new(server.stderr.take().unwrap());
    let mut log = String::new();
    let port = loop {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = server.wait();
            panic!("the server exited before listening:\n{log}");
        }
        log.push_str(&line);
        if let Some(address) = line
            .trim_end()
            .strip_prefix("wasm-dap: listening for DAP clients on ")
        {
            let (_, port) = address.rsplit_once(':').expect("host:port");
            break port.parse::<u16>().expect("port number");
        }
    };
    let rest = std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = stderr.read_to_string(&mut rest);
        rest
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tcp_sessions(port, &mut server)
    }));
    let status = server.try_wait().ok().flatten();
    let _ = server.kill();
    let _ = server.wait();
    log.push_str(&rest.join().unwrap_or_default());
    if let Err(panic) = outcome {
        eprintln!("server status: {status:?}\nserver stderr:\n{log}");
        std::panic::resume_unwind(panic);
    }
}

/// An authenticated session followed by clients that do not know the token.
fn tcp_sessions(port: u16, server: &mut Child) {
    let mut connect = || {
        if let Some(status) = server.try_wait().unwrap() {
            panic!("the server exited with {status}");
        }
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the server");
        DapClient::from_streams(stream.try_clone().unwrap(), stream)
    };

    let mut client = connect();
    client.launch(json!({
        "program": arith(),
        "invoke": "fib",
        "invokeArgs": [6],
        "serverToken": "e2e-secret",
    }));
    client.ok("configurationDone", json!({}));
    assert!(client.wait_output("fib(6) = 8").contains("= 8"));
    client.event("terminated");
    client.disconnect();

    // Clients that do not know the token can neither read files nor launch.
    let mut intruder = connect();
    intruder.ok("initialize", json!({}));
    let source = source_path("arith.rs");
    let refused = intruder.fails("source", json!({ "source": { "path": source } }));
    assert!(refused.contains("authentication required"), "{refused}");
    let refused = intruder.fails(
        "launch",
        json!({ "program": arith(), "invoke": "fib", "invokeArgs": [1], "serverToken": "guess" }),
    );
    assert!(refused.contains("serverToken"), "{refused}");
    assert!(
        !intruder.has_event_within("initialized", Duration::from_millis(200)),
        "a rejected launch must not start a session"
    );

    let mut without_token = connect();
    without_token.ok("initialize", json!({}));
    let refused = without_token.fails(
        "launch",
        json!({ "program": arith(), "invoke": "fib", "invokeArgs": [1] }),
    );
    assert!(refused.contains("serverToken"), "{refused}");
}

#[test]
fn info_describes_module_structure() {
    let path = arith();
    let output = run(&["info", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("DWARF"), "{text}");
    assert!(text.contains("Exports ("), "{text}");
    assert!(text.contains("arith::fib (i32) -> i32"), "{text}");
    assert!(text.contains("/fixtures/arith.rs"), "{text}");

    let output = run(&["info", path.to_str().unwrap(), "--json"]);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(report["debugInfo"]["dwarf"], true);
    let functions = report["functions"].as_array().unwrap();
    let fib = functions
        .iter()
        .find(|f| f["name"] == "arith::fib")
        .unwrap();
    assert_eq!(fib["signature"], "(i32) -> i32");
    assert!(
        fib["location"]
            .as_str()
            .unwrap()
            .contains("/fixtures/arith.rs:")
    );
    // Older rustc versions also export `__data_end` and `__heap_base`.
    let exports: Vec<&str> = report["exports"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    for name in [
        "memory",
        "bump",
        "divide",
        "fib",
        "shapes",
        "spin",
        "sum_points",
    ] {
        assert!(exports.contains(&name), "{name} missing from {exports:?}");
    }
}

#[test]
fn disasm_annotates_instructions_with_source_lines() {
    let path = arith();
    let output = run(&["disasm", path.to_str().unwrap(), "--function", "fib"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.starts_with("func["), "{text}");
    assert!(text.contains("arith::fib"), "{text}");
    assert!(text.contains("call $fib"), "{text}");
    assert!(
        text.contains(&format!("arith.rs:{}", line_of("arith.rs", "BP: fib-sum"))),
        "{text}"
    );

    let missing = run(&[
        "disasm",
        path.to_str().unwrap(),
        "--function",
        "nothing_here",
    ]);
    assert!(!missing.status.success());
}

#[test]
fn trace_records_calls_and_returns() {
    let path = arith();
    let output = run(&["trace", path.to_str().unwrap(), "--invoke", "fib", "3"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout(&output);
    assert!(text.contains("→ arith::fib(3)"), "{text}");
    assert!(text.contains("  → arith::fib(2)"), "{text}");
    assert!(text.contains("← arith::fib = 2"), "{text}");
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.contains("returned 2"), "{summary}");
    assert!(summary.contains("5 calls"), "{summary}");
}

#[test]
fn trace_json_captures_wasi_output_and_exit_code() {
    let program = rust_fixture("wasi_app.rs", RustTarget::Wasi);
    let output = run(&[
        "trace",
        program.to_str().unwrap(),
        "--json",
        "--max-events",
        "100000",
        "--env",
        "GREETING_NAME=trace",
        "--",
        "--fail",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let events = report["events"].as_array().unwrap();
    let printed: String = events
        .iter()
        .filter(|e| e["kind"] == "output")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert!(printed.contains("hello, trace!"), "{printed}");
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "call" && e["function"] == "wasi_app::factorial")
    );
    assert_eq!(report["summary"]["exit"]["status"], "exited");
    assert_eq!(report["summary"]["exit"]["code"], 3);
}

#[test]
fn trace_reports_traps_with_backtraces() {
    let path = arith();
    let output = run(&[
        "trace",
        path.to_str().unwrap(),
        "--invoke",
        "divide",
        "1",
        "0",
    ]);
    assert!(!output.status.success());
    let text = stdout(&output);
    assert!(text.contains("trap:"), "{text}");
    assert!(text.contains("arith::divide"), "{text}");
}

#[test]
fn invalid_input_is_rejected_with_a_message() {
    let bogus = std::env::temp_dir().join(format!("wasm-dap-bogus-{}.wasm", std::process::id()));
    std::fs::write(&bogus, b"definitely not wasm").unwrap();
    let output = run(&["info", bogus.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    let _ = std::fs::remove_file(&bogus);
}

/// Builds `examples/rust-wasi` with Cargo, exactly as a user would.
fn cargo_example() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root.join("examples/rust-wasi/Cargo.toml");
    let target_dir = root.join("target/examples");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--quiet",
            "--target",
            "wasm32-wasip1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo");
    assert!(
        status.success(),
        "building examples/rust-wasi failed (is wasm32-wasip1 installed?)"
    );
    target_dir.join("wasm32-wasip1/debug/word-count.wasm")
}

#[test]
fn debugs_a_cargo_built_wasi_program() {
    let program = cargo_example();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/rust-wasi/src/main.rs")
        .canonicalize()
        .unwrap();
    let text = std::fs::read_to_string(&source).unwrap();
    let line = text
        .lines()
        .position(|l| l.contains("match word.len()"))
        .unwrap() as u32
        + 1;

    let mut client = DapClient::spawn(&binary());
    client.launch(json!({
        "program": program,
        "args": ["the", "cat", "sat", "on", "the", "magnificent", "mat"],
    }));
    let body = client.ok(
        "setBreakpoints",
        json!({
            "source": { "path": source },
            "breakpoints": [{ "line": line, "condition": "word.len() > 7" }]
        }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    let stopped = client.stopped();
    assert_eq!(stopped["reason"], "breakpoint", "{stopped}");
    let frames = client.ok("stackTrace", json!({ "threadId": 1 }));
    let frames = frames["stackFrames"].as_array().unwrap();
    assert_eq!(frames[0]["name"], "word_count::categorize");
    assert!(
        frames
            .iter()
            .any(|f| f["name"] == "word_count::count_words")
    );
    let top = frames[0]["id"].clone();
    let word = client.ok("evaluate", json!({ "expression": "word", "frameId": top }));
    assert_eq!(word["result"], "\"magnificent\"");
    client.disconnect();
}
