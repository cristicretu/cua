//! The dispatch seam gives every mutating tool's result, and every error, a
//! `disposition`: whether input reached the app and whether a retry is safe.

use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{
    EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
};
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// A stand-in tool that returns whatever its `script` argument names.
struct Scripted(ToolDef);

#[async_trait::async_trait]
impl Tool for Scripted {
    fn def(&self) -> &ToolDef {
        &self.0
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match args.get("script").and_then(Value::as_str) {
            // A code this test, and the disposition table, never named.
            Some("refused") => {
                ToolResult::error("refused").with_structured(json!({"code": "some_future_refusal"}))
            }
            Some("failed_write") => ToolResult::error("AXPress returned kAXErrorFailure")
                .with_structured(json!({"code": "ax_action_failed"})),
            Some("bare_error") => ToolResult::error("something went wrong"),
            Some("missing") => ToolResult::error("Missing required parameter: from_x"),
            Some("confirmed") => ToolResult::text("Set value").with_structured(json!({
                "path": "ax",
                "verified": true,
                "effect": "confirmed",
                "evidence": [{"kind": "accessibility_readback"}],
            })),
            _ => ToolResult::text("done"),
        }
    }
}

fn unrestricted_context() -> Arc<EffectiveAuthorizationContext> {
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap()
}

async fn call(tool: &str, args: Value) -> ToolResult {
    let mut registry = ToolRegistry::new();
    for (name, read_only) in [("click", false), ("get_window_state", true)] {
        registry.register(Box::new(Scripted(ToolDef {
            name: name.to_owned(),
            description: "stub".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script": {"type": "string"},
                    "pid": {"type": "integer"},
                    "window_id": {"type": "integer"},
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                },
            }),
            read_only,
            destructive: false,
            idempotent: false,
            open_world: false,
        })));
    }
    let registry = Arc::new(registry);
    registry.init_self_weak();
    registry
        .invoke_with_context(tool, args, unrestricted_context())
        .await
}

fn disposition(result: &ToolResult) -> Value {
    result.structured_content.as_ref().expect("structured")["disposition"].clone()
}

const NOT_DELIVERED: &str = r#"{"delivery":"not_delivered","retry":"safe"}"#;

fn parsed(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn any_coded_refusal_says_nothing_was_sent() {
    let result = call(
        "click",
        json!({"script": "refused", "pid": 1, "window_id": 2}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(disposition(&result), parsed(NOT_DELIVERED));
}

#[tokio::test]
async fn a_plain_argument_error_is_not_delivered() {
    let result = call("click", json!({"script": "missing"})).await;
    assert_eq!(disposition(&result), parsed(NOT_DELIVERED));
}

#[tokio::test]
async fn a_failed_write_is_uncertain_and_a_bare_error_unknown() {
    let result = call(
        "click",
        json!({"script": "failed_write", "pid": 1, "window_id": 2}),
    )
    .await;
    assert_eq!(
        disposition(&result),
        json!({"delivery": "delivery_uncertain", "retry": "unsafe"})
    );
    let result = call("click", json!({"script": "bare_error"})).await;
    assert_eq!(
        disposition(&result),
        json!({"delivery": "unknown", "retry": "unknown"})
    );
}

#[tokio::test]
async fn a_successful_action_publishes_its_disposition_inside_the_closed_contract() {
    for (script, expected) in [
        ("confirmed", "delivered_verified"),
        ("plain", "delivered_unverified"),
    ] {
        let result = call("click", json!({"script": script, "pid": 1, "window_id": 2})).await;
        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured["disposition"],
            json!({"delivery": expected, "retry": "unsafe"}),
            "{script}"
        );
        let schema = cua_driver_contract::advertised_tool_output_schema("click").expect("schema");
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        assert!(
            validator.is_valid(&structured),
            "schema rejected {structured}"
        );
        cua_driver_contract::validate_success_output("click", structured).expect("typed output");
    }
}

#[tokio::test]
async fn reads_are_not_delivered_on_error_and_untouched_on_success() {
    let result = call("get_window_state", json!({"script": "bare_error"})).await;
    assert_eq!(disposition(&result), parsed(NOT_DELIVERED));
    let result = call("get_window_state", json!({"pid": 1, "window_id": 2})).await;
    assert!(result.structured_content.is_none());
}
