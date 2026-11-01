use serde_json::json;
use wasm_dap_protocol::{Event, Message, Request, Response};

#[test]
fn request_round_trips_through_json() {
    let original = Message::Request(Request {
        seq: 1,
        command: "initialize".to_string(),
        arguments: Some(json!({"clientID": "test"})),
    });
    let encoded = original.to_json().unwrap();
    let decoded = Message::from_json(&encoded).unwrap();
    match decoded {
        Message::Request(r) => {
            assert_eq!(r.seq, 1);
            assert_eq!(r.command, "initialize");
            assert!(r.arguments.is_some());
        }
        _ => panic!("expected Request"),
    }
}

#[test]
fn response_round_trips_through_json() {
    let original = Message::Response(Response {
        seq: 2,
        request_seq: 1,
        success: true,
        command: "initialize".to_string(),
        message: None,
        body: Some(json!({"supportsConfigurationDoneRequest": true})),
    });
    let encoded = original.to_json().unwrap();
    let decoded = Message::from_json(&encoded).unwrap();
    match decoded {
        Message::Response(r) => {
            assert!(r.success);
            assert_eq!(r.request_seq, 1);
            assert!(r.message.is_none());
            assert!(r.body.is_some());
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn event_round_trips_through_json() {
    let original = Message::Event(Event {
        seq: 3,
        event: "initialized".to_string(),
        body: None,
    });
    let encoded = original.to_json().unwrap();
    let decoded = Message::from_json(&encoded).unwrap();
    match decoded {
        Message::Event(e) => {
            assert_eq!(e.event, "initialized");
            assert!(e.body.is_none());
        }
        _ => panic!("expected Event"),
    }
}
