//! Debugging C and Zig programs built with Zig's toolchain (clang for C).
//!
//! These tests run when `WASM_DAP_ZIG` points to a `zig` executable and are
//! skipped otherwise.

use std::path::PathBuf;

use serde_json::{Value, json};
use wasm_dap_testkit::fixtures::{source_path, zig, zig_fixture};
use wasm_dap_testkit::{DapClient, line_of};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wasm-dap"))
}

fn evaluate(client: &mut DapClient, frame: &Value, expression: &str) -> String {
    let body = client.ok(
        "evaluate",
        json!({ "expression": expression, "frameId": frame, "context": "watch" }),
    );
    body["result"].as_str().unwrap().to_string()
}

#[test]
fn c_programs_are_debugged_at_the_source_level() {
    let Some(zig) = zig() else {
        eprintln!("skipped: set WASM_DAP_ZIG to a zig executable to test C programs");
        return;
    };
    let program = zig_fixture(&zig, "c_app.c");
    let source = source_path("c_app.c");
    let line = line_of("c_app.c", "BP: c-distance");

    let mut client = DapClient::spawn(&binary());
    client.launch(json!({ "program": program, "args": ["one", "two"] }));
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source }, "breakpoints": [{ "line": line }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");

    let trace = client.ok("stackTrace", json!({ "threadId": 1 }));
    let frames = trace["stackFrames"].as_array().unwrap();
    assert_eq!(frames[0]["name"], "distance2");
    assert_eq!(frames[0]["line"], line);
    assert_eq!(frames[1]["name"], "main");
    let (top, caller) = (frames[0]["id"].clone(), frames[1]["id"].clone());

    assert_eq!(evaluate(&mut client, &top, "a->x"), "1");
    assert_eq!(evaluate(&mut client, &top, "b->y"), "6");
    assert_eq!(evaluate(&mut client, &top, "dx"), "-3");
    assert_eq!(evaluate(&mut client, &top, "dx * dx + dy * dy"), "25");
    assert_eq!(evaluate(&mut client, &top, "*b"), "{x: 4, y: 6}");
    assert_eq!(evaluate(&mut client, &caller, "points[2].x"), "-3");
    assert_eq!(evaluate(&mut client, &caller, "shade"), "GREEN");
    assert_eq!(evaluate(&mut client, &caller, "label"), "\"points\"");
    assert_eq!(evaluate(&mut client, &caller, "counter"), "0");
    assert_eq!(evaluate(&mut client, &caller, "argc"), "3");
    assert!(evaluate(&mut client, &caller, "greeting").ends_with("\"hello from C\""));

    client.ok("continue", json!({ "threadId": 1 }));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert_eq!(evaluate(&mut client, &top["id"], "dx"), "4");

    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source }, "breakpoints": [] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    let output = client.wait_output("points: total=45 shade=5 argc=3 hello from C");
    assert!(output.contains("total=45"));
    assert_eq!(client.event("exited").body.unwrap()["exitCode"], 0);
    client.event("terminated");
    client.disconnect();
}

#[test]
fn zig_programs_are_debugged_at_the_source_level() {
    let Some(zig) = zig() else {
        eprintln!("skipped: set WASM_DAP_ZIG to a zig executable to test Zig programs");
        return;
    };
    let program = zig_fixture(&zig, "zig_app.zig");
    let source = source_path("zig_app.zig");
    let line = line_of("zig_app.zig", "BP: zig-combine");

    let mut client = DapClient::spawn(&binary());
    client.launch(json!({ "program": program }));
    let body = client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source }, "breakpoints": [{ "line": line }] }),
    );
    assert_eq!(body["breakpoints"][0]["verified"], true, "{body}");
    client.ok("configurationDone", json!({}));
    assert_eq!(client.stopped()["reason"], "breakpoint");
    let top = client.top_frame();
    assert!(top["name"].as_str().unwrap().ends_with("combine"), "{top}");
    assert_eq!(top["line"], line);
    assert_eq!(evaluate(&mut client, &top["id"], "p.a + p.b"), "3");
    assert_eq!(evaluate(&mut client, &top["id"], "scale"), "3");
    assert_eq!(evaluate(&mut client, &top["id"], "sum"), "3");

    client.ok(
        "setBreakpoints",
        json!({ "source": { "path": source }, "breakpoints": [] }),
    );
    client.ok("continue", json!({ "threadId": 1 }));
    assert!(client.wait_output("total=99").contains("total=99"));
    assert_eq!(client.event("exited").body.unwrap()["exitCode"], 0);
    client.event("terminated");
    client.disconnect();
}
