//! macOS semantic state tools at the wire, without any desktop input: calls
//! that name no live element are refused by the real handlers before any
//! Accessibility work, directly and as `run_steps` steps.
#![cfg(target_os = "macos")]

use cua_driver_testkit::RawDriver;
use serde_json::json;

#[test]
fn semantic_state_tools_refuse_without_a_live_element_directly_and_in_a_batch() {
    let mut driver =
        RawDriver::spawn_explicit_direct().expect("spawn source-built direct MCP driver");
    driver.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    assert!(driver.recv()["result"].is_object());
    let calls = [
        ("set_state", json!({"pid":1,"window_id":1,"state":"checked"})),
        ("clear_value", json!({"pid":1,"window_id":1})),
        ("scroll_to", json!({"pid":1,"window_id":1})),
    ];
    for (tool, arguments) in calls {
        let expected = format!("{tool} requires element_token to address the target element.");
        driver.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":tool,"arguments":arguments
        }}));
        let direct = driver.recv();
        assert_eq!(direct["result"]["isError"], true, "{direct}");
        assert_eq!(direct["result"]["content"][0]["text"], expected, "{direct}");

        driver.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"run_steps","arguments":{"steps":[{"tool":tool,"args":arguments}]}
        }}));
        let batch = driver.recv();
        let result = &batch["result"]["structuredContent"];
        assert_ne!(result["code"], "invalid_batch", "{batch}");
        assert_eq!(result["steps"][0]["phase"], "action", "{batch}");
        assert_eq!(result["steps"][0]["message"], expected, "{batch}");
    }

    // A token the runtime never minted names no pid: stale, nothing sent.
    driver.send(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
        "name":"set_state","arguments":{"element_token":"s0000beef:3","state":"expanded"}
    }}));
    let stale = driver.recv();
    assert_eq!(stale["result"]["isError"], true, "{stale}");
    assert_eq!(
        stale["result"]["structuredContent"]["refusal"]["code"], "stale_element_token",
        "{stale}"
    );

    driver.send(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
        "name":"set_state","arguments":{"pid":1,"window_id":1,"state":"visible"}
    }}));
    let unknown = driver.recv();
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");
}
