use std::io::Cursor;

use wasm_dap_protocol::message::{Event, Message, Request};
use wasm_dap_protocol::transport::{FrameReader, FrameWriter, TransportError};

#[test]
fn writes_and_reads_request() {
    let original = Message::Request(Request {
        seq: 1,
        command: "initialize".to_string(),
        arguments: None,
    });

    let mut buf = Vec::new();
    FrameWriter::new(&mut buf).write_message(&original).unwrap();

    let mut reader = FrameReader::new(Cursor::new(&buf[..]));
    let read = reader.read_message().unwrap().unwrap();

    match read {
        Message::Request(r) => {
            assert_eq!(r.seq, 1);
            assert_eq!(r.command, "initialize");
        }
        _ => panic!("expected Request"),
    }
}

#[test]
fn round_trips_event_with_body() {
    let original = Message::Event(Event {
        seq: 7,
        event: "stopped".to_string(),
        body: Some(serde_json::json!({"reason": "breakpoint"})),
    });

    let mut buf = Vec::new();
    FrameWriter::new(&mut buf).write_message(&original).unwrap();

    let mut reader = FrameReader::new(Cursor::new(&buf[..]));
    let read = reader.read_message().unwrap().unwrap();

    match read {
        Message::Event(e) => {
            assert_eq!(e.seq, 7);
            assert_eq!(e.event, "stopped");
            assert!(e.body.is_some());
        }
        _ => panic!("expected Event"),
    }
}

#[test]
fn reads_clean_eof_as_none() {
    let empty: &[u8] = b"";
    let mut reader = FrameReader::new(Cursor::new(empty));
    assert!(reader.read_message().unwrap().is_none());
}

#[test]
fn rejects_missing_content_length() {
    let input: &[u8] = b"Content-Type: application/json\r\n\r\n{}";
    let mut reader = FrameReader::new(Cursor::new(input));
    let err = reader.read_message().unwrap_err();
    assert!(matches!(err, TransportError::MissingContentLength));
}

#[test]
fn rejects_invalid_content_length() {
    let input: &[u8] = b"Content-Length: abc\r\n\r\n";
    let mut reader = FrameReader::new(Cursor::new(input));
    let err = reader.read_message().unwrap_err();
    assert!(matches!(err, TransportError::InvalidContentLength(_)));
}

#[test]
fn rejects_oversized_frames() {
    let endless_header = vec![b'x'; 64 * 1024];
    let mut reader = FrameReader::new(Cursor::new(&endless_header[..]));
    let err = reader.read_message().unwrap_err();
    assert!(matches!(err, TransportError::HeaderTooLong), "{err}");

    let input: &[u8] = b"Content-Length: 1000000000000\r\n\r\n";
    let mut reader = FrameReader::new(Cursor::new(input));
    let err = reader.read_message().unwrap_err();
    assert!(matches!(err, TransportError::MessageTooLarge(_)), "{err}");
}

#[test]
fn ignores_unknown_headers() {
    let body = br#"{"seq":1,"type":"event","event":"initialized"}"#;
    let input = format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut combined = input.into_bytes();
    combined.extend_from_slice(body);

    let mut reader = FrameReader::new(Cursor::new(&combined[..]));
    let msg = reader.read_message().unwrap().unwrap();
    match msg {
        Message::Event(e) => assert_eq!(e.event, "initialized"),
        _ => panic!("expected Event"),
    }
}
