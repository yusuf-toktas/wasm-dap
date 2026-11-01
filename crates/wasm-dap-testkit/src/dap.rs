//! A synchronous Debug Adapter Protocol client for driving real adapters.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wasm_dap_protocol::message::{Event, Message, Request, Response};
use wasm_dap_protocol::transport::{FrameReader, FrameWriter};

pub struct DapClient {
    writer: FrameWriter<Box<dyn Write + Send>>,
    incoming: Receiver<Message>,
    buffer: VecDeque<Message>,
    seq: i64,
    child: Option<Child>,
    timeout: Duration,
}

impl DapClient {
    /// Launches `<binary> serve` and talks to it over stdio.
    pub fn spawn(binary: &Path) -> Self {
        let mut child = Command::new(binary)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", binary.display()));
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let mut client = Self::from_streams(stdout, stdin);
        client.child = Some(child);
        client
    }

    /// Talks to an adapter through arbitrary byte streams.
    pub fn from_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = FrameReader::new(BufReader::new(reader));
            while let Ok(Some(message)) = reader.read_message() {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            writer: FrameWriter::new(Box::new(writer)),
            incoming: rx,
            buffer: VecDeque::new(),
            seq: 0,
            child: None,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Sends a request and returns its sequence number.
    pub fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        let arguments = (!arguments.is_null()).then_some(arguments);
        let message = Message::Request(Request {
            seq: self.seq,
            command: command.to_string(),
            arguments,
        });
        self.writer
            .write_message(&message)
            .unwrap_or_else(|e| panic!("failed to send {command}: {e}"));
        self.seq
    }

    fn receive(&mut self, deadline: Instant, what: &str) -> Message {
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        match self.incoming.recv_timeout(remaining) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => panic!(
                "timed out waiting for {what}; unconsumed messages: {:#?}",
                self.buffer
            ),
            Err(RecvTimeoutError::Disconnected) => panic!(
                "adapter closed the connection while waiting for {what}; unconsumed: {:#?}",
                self.buffer
            ),
        }
    }

    /// Waits for the response to request `seq`.
    pub fn response(&mut self, seq: i64) -> Response {
        if let Some(pos) = self
            .buffer
            .iter()
            .position(|m| matches!(m, Message::Response(r) if r.request_seq == seq))
            && let Some(Message::Response(r)) = self.buffer.remove(pos)
        {
            return r;
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.receive(deadline, &format!("response to request {seq}")) {
                Message::Response(r) if r.request_seq == seq => return r,
                other => self.buffer.push_back(other),
            }
        }
    }

    /// Sends a request and waits for its response.
    pub fn request(&mut self, command: &str, arguments: Value) -> Response {
        let seq = self.send(command, arguments);
        self.response(seq)
    }

    /// Sends a request, asserts success and returns the response body.
    pub fn ok(&mut self, command: &str, arguments: Value) -> Value {
        let response = self.request(command, arguments);
        assert!(
            response.success,
            "{command} failed: {}",
            response.message.unwrap_or_default()
        );
        response.body.unwrap_or(Value::Null)
    }

    /// Sends a request, asserts failure and returns the error message.
    pub fn fails(&mut self, command: &str, arguments: Value) -> String {
        let response = self.request(command, arguments);
        assert!(
            !response.success,
            "{command} unexpectedly succeeded: {:?}",
            response.body
        );
        response.message.unwrap_or_default()
    }

    /// Waits for the next event with the given name.
    pub fn event(&mut self, name: &str) -> Event {
        if let Some(pos) = self
            .buffer
            .iter()
            .position(|m| matches!(m, Message::Event(e) if e.event == name))
            && let Some(Message::Event(e)) = self.buffer.remove(pos)
        {
            return e;
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.receive(deadline, &format!("event `{name}`")) {
                Message::Event(e) if e.event == name => return e,
                other => self.buffer.push_back(other),
            }
        }
    }

    /// Body of the next `stopped` event.
    pub fn stopped(&mut self) -> Value {
        self.event("stopped").body.unwrap_or(Value::Null)
    }

    /// Returns true if an event with that name arrives within `wait`.
    pub fn has_event_within(&mut self, name: &str, wait: Duration) -> bool {
        if self
            .buffer
            .iter()
            .any(|m| matches!(m, Message::Event(e) if e.event == name))
        {
            return true;
        }
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            match self
                .incoming
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(message) => {
                    let matched = matches!(&message, Message::Event(e) if e.event == name);
                    self.buffer.push_back(message);
                    if matched {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    /// Concatenated text of all `output` events received so far.
    pub fn output(&mut self) -> String {
        while let Ok(message) = self.incoming.try_recv() {
            self.buffer.push_back(message);
        }
        self.buffer
            .iter()
            .filter_map(|m| match m {
                Message::Event(e) if e.event == "output" => e
                    .body
                    .as_ref()
                    .and_then(|b| b["output"].as_str())
                    .map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// Waits until the accumulated output contains `needle`.
    pub fn wait_output(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + self.timeout;
        loop {
            let text = self.output();
            if text.contains(needle) {
                return text;
            }
            let message = self.receive(deadline, &format!("output containing {needle:?}"));
            self.buffer.push_back(message);
        }
    }

    /// Performs `initialize` and `launch`, then waits for `initialized`.
    pub fn launch(&mut self, arguments: Value) -> Value {
        let capabilities = self.ok(
            "initialize",
            json!({
                "clientID": "wasm-dap-testkit",
                "adapterID": "wasm-dap",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsMemoryReferences": true
            }),
        );
        self.ok("launch", arguments);
        self.event("initialized");
        capabilities
    }

    /// Top stack frame body of the stopped thread.
    pub fn top_frame(&mut self) -> Value {
        let body = self.ok("stackTrace", json!({ "threadId": 1 }));
        body["stackFrames"][0].clone()
    }

    /// Variables of the named scope in the given frame, as (name, value) pairs.
    pub fn scope_variables(&mut self, frame_id: i64, scope: &str) -> Vec<Value> {
        let scopes = self.ok("scopes", json!({ "frameId": frame_id }));
        let reference = scopes["scopes"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["name"] == scope))
            .map(|s| s["variablesReference"].clone())
            .unwrap_or_else(|| panic!("scope {scope} not found in {scopes}"));
        let body = self.ok("variables", json!({ "variablesReference": reference }));
        body["variables"].as_array().cloned().unwrap_or_default()
    }

    /// Ends the session and waits for the adapter to exit.
    pub fn disconnect(&mut self) {
        let seq = self.send("disconnect", json!({ "terminateDebuggee": true }));
        let response = self.response(seq);
        assert!(
            response.success,
            "disconnect failed: {:?}",
            response.message
        );
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        assert!(
                            status.success(),
                            "adapter exited with {status} after disconnect"
                        );
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => {
                        let _ = child.kill();
                        panic!("adapter did not exit after disconnect");
                    }
                }
            }
        }
    }

    /// All messages received but not consumed yet.
    pub fn pending(&mut self) -> Vec<Message> {
        while let Ok(message) = self.incoming.try_recv() {
            self.buffer.push_back(message);
        }
        self.buffer.iter().cloned().collect()
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Finds a variable by name in a `variables` response list.
pub fn find_var<'a>(variables: &'a [Value], name: &str) -> &'a Value {
    variables
        .iter()
        .find(|v| v["name"] == name)
        .unwrap_or_else(|| panic!("variable {name} not found in {variables:#?}"))
}

/// Response-or-event helper used by assertions.
pub fn event_body(event: &Event) -> &Value {
    event.body.as_ref().unwrap_or(&Value::Null)
}
