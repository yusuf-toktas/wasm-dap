use serde_json::Value;
use wasm_dap_protocol::Capabilities;

#[test]
fn capabilities_serialize_with_dap_camel_case_keys() {
    let value = serde_json::to_value(Capabilities::default()).unwrap();
    let object = value
        .as_object()
        .expect("capabilities must serialize to a JSON object");

    for key in [
        "supportsConfigurationDoneRequest",
        "supportsFunctionBreakpoints",
        "supportsConditionalBreakpoints",
        "supportsHitConditionalBreakpoints",
        "supportsEvaluateForHovers",
        "supportsLoadedSourcesRequest",
        "supportsSetVariableRequest",
        "supportsReadMemoryRequest",
    ] {
        assert!(
            object.contains_key(key),
            "missing DAP capability key: {key}"
        );
    }

    for key in [
        "supports_configuration_done_request",
        "supports_function_breakpoints",
        "supports_conditional_breakpoints",
        "supports_hit_conditional_breakpoints",
        "supports_evaluate_for_hovers",
        "supports_loaded_sources_request",
        "supports_set_variable_request",
        "supports_read_memory_request",
    ] {
        assert!(!object.contains_key(key), "snake_case key leaked: {key}");
    }
}

#[test]
fn capabilities_deserialize_from_dap_camel_case_payload() {
    let payload: Value = serde_json::json!({
        "supportsConfigurationDoneRequest": true,
        "supportsFunctionBreakpoints": false,
        "supportsConditionalBreakpoints": true,
        "supportsHitConditionalBreakpoints": false,
        "supportsEvaluateForHovers": true,
        "supportsLoadedSourcesRequest": false,
        "supportsSetVariableRequest": true,
        "supportsReadMemoryRequest": false,
    });

    let capabilities: Capabilities = serde_json::from_value(payload).unwrap();
    assert!(capabilities.supports_configuration_done_request);
    assert!(!capabilities.supports_function_breakpoints);
    assert!(capabilities.supports_conditional_breakpoints);
    assert!(!capabilities.supports_hit_conditional_breakpoints);
    assert!(capabilities.supports_evaluate_for_hovers);
    assert!(!capabilities.supports_loaded_sources_request);
    assert!(capabilities.supports_set_variable_request);
    assert!(!capabilities.supports_read_memory_request);
}
