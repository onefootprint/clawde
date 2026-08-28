//! Tests for the in-process SDK MCP server, ported from the Python SDK's
//! `test_sdk_mcp_integration.py` behaviors.

use claude_agent_sdk::mcp::{
    create_sdk_mcp_server, tool, CallToolResult, ToolAnnotations, ToolResultContent,
};
use claude_agent_sdk::ClaudeSdkError;
use serde_json::{json, Value};

fn calculator() -> claude_agent_sdk::McpSdkServerConfig {
    let add = tool(
        "add",
        "Add two numbers",
        json!({"a": {"type": "number"}, "b": {"type": "number"}}),
        |args| async move {
            let a = args["a"].as_f64().unwrap_or(0.0);
            let b = args["b"].as_f64().unwrap_or(0.0);
            Ok(CallToolResult::text(format!("Sum: {}", a + b)))
        },
    );
    let fail = tool("fail", "Always fails", json!({}), |_args| async move {
        Err::<CallToolResult, _>(ClaudeSdkError::other("Division by zero"))
    });
    let annotated = tool("schema", "Get schema", json!({}), |_args| async move {
        Ok(CallToolResult {
            content: vec![
                ToolResultContent::text("big"),
                ToolResultContent::ResourceLink {
                    name: Some("db".to_string()),
                    uri: Some("res://db".to_string()),
                    description: None,
                },
            ],
            is_error: false,
        })
    })
    .with_annotations(ToolAnnotations {
        read_only_hint: Some(true),
        max_result_size_chars: Some(500_000),
        ..Default::default()
    });
    create_sdk_mcp_server("calculator", "2.0.0", vec![add, fail, annotated])
}

async fn call(server: &claude_agent_sdk::McpSdkServerConfig, message: Value) -> Option<Value> {
    server.instance.handle_message(message).await.unwrap()
}

#[tokio::test]
async fn initialize_handshake() {
    let server = calculator();
    let response = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-01-01", "capabilities": {}},
        }),
    )
    .await
    .expect("initialize response");
    assert_eq!(response["result"]["protocolVersion"], "2025-01-01");
    assert_eq!(response["result"]["serverInfo"]["name"], "calculator");
    assert_eq!(response["result"]["serverInfo"]["version"], "2.0.0");

    // Notifications get no reply.
    assert!(call(
        &server,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .await
    .is_none());

    // Ping is answered.
    let pong = call(
        &server,
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
    )
    .await
    .unwrap();
    assert_eq!(pong["result"], json!({}));
}

#[tokio::test]
async fn tools_list_includes_annotations_and_meta() {
    let server = calculator();
    let response = call(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await
    .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    let add = &tools[0];
    assert_eq!(add["name"], "add");
    assert_eq!(add["inputSchema"]["type"], "object");
    assert_eq!(add["inputSchema"]["required"], json!(["a", "b"]));

    let schema_tool = tools.iter().find(|t| t["name"] == "schema").unwrap();
    assert_eq!(schema_tool["annotations"]["readOnlyHint"], true);
    // maxResultSizeChars travels in the tool's _meta under a namespaced key
    // because MCP clients drop unknown annotation fields.
    assert_eq!(
        schema_tool["_meta"]["anthropic/maxResultSizeChars"],
        500_000
    );
}

#[tokio::test]
async fn tools_call_executes_and_validates() {
    let server = calculator();
    let ok = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "add", "arguments": {"a": 2, "b": 3}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(ok["result"]["isError"], false);
    assert_eq!(ok["result"]["content"][0]["text"], "Sum: 5");

    // Invalid arguments come back as an isError result, not a protocol
    // error.
    let invalid = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "add", "arguments": {"a": "not a number", "b": 3}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(invalid["result"]["isError"], true);
    let text = invalid["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Input validation error:"), "{text}");

    // Unknown tools too.
    let unknown = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "nope", "arguments": {}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(unknown["result"]["isError"], true);
    assert_eq!(
        unknown["result"]["content"][0]["text"],
        "Tool 'nope' not found"
    );

    // Handler errors too.
    let failed = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "fail", "arguments": {}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(failed["result"]["isError"], true);
    assert_eq!(failed["result"]["content"][0]["text"], "Division by zero");

    // Resource links are flattened to text.
    let flattened = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "schema", "arguments": {}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(flattened["result"]["content"][1]["type"], "text");
    assert_eq!(flattened["result"]["content"][1]["text"], "db\nres://db");
}

#[tokio::test]
async fn unknown_methods_are_method_not_found() {
    let server = calculator();
    let response = call(
        &server,
        json!({"jsonrpc": "2.0", "id": 9, "method": "resources/list"}),
    )
    .await
    .unwrap();
    assert_eq!(response["error"]["code"], -32601);
}

#[tokio::test]
async fn invalid_schema_never_runs_handler_unvalidated() {
    // Regression test (codex round-3 finding): a tool declaring an invalid
    // JSON Schema must fail calls as an isError result WITHOUT running the
    // handler — never silently skip validation. Matches the Python SDK,
    // where jsonschema raises inside the guarded call path.
    let server = create_sdk_mcp_server(
        "srv",
        "1.0.0",
        vec![tool(
            "bad_schema",
            "Bad schema",
            json!({
                "type": "object",
                "properties": {"x": {"type": "definitely-not-a-json-schema-type"}},
            }),
            |_args| async move { Ok(CallToolResult::text("handler-ran")) },
        )],
    );
    let response = call(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "bad_schema", "arguments": {"x": 1}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Input validation error:"), "{text}");
    assert_ne!(text, "handler-ran");
}
