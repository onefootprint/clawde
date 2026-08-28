//! End-to-end control-protocol tests against a fake CLI: inbound
//! `can_use_tool`, `hook_callback`, and `mcp_message` control requests.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use claude_agent_sdk::mcp::{create_sdk_mcp_server, tool, CallToolResult};
use claude_agent_sdk::{
    query, ClaudeAgentOptions, HookEvent, HookInput, HookJsonOutput, HookMatcher,
    HookSpecificOutput, McpServerConfig, McpServers, Message, PermissionDecision, PermissionResult,
    PermissionResultAllow, PermissionUpdate, SyncHookJsonOutput,
};
use futures::StreamExt;
use serde_json::json;

fn fake_cli(body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("claude");
    let script = format!(
        "#!/bin/bash\n\
         if [ \"$1\" = \"-v\" ]; then echo \"2.1.0 (fake)\"; exit 0; fi\n\
         {body}\n"
    );
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(script.as_bytes()).expect("write script");
    drop(file);
    let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    (dir, path)
}

async fn run_and_get_result_text(options: ClaudeAgentOptions, prompt: &str) -> String {
    let mut messages = query(prompt, options).await.expect("query");
    let mut result_text = String::new();
    while let Some(message) = messages.next().await {
        if let Message::Result(result) = message.expect("message") {
            result_text = result.result.unwrap_or_default();
        }
    }
    result_text
}

#[tokio::test]
async fn can_use_tool_and_mcp_message_round_trip() {
    // The fake CLI sends a can_use_tool permission request and an SDK MCP
    // initialize, checks both responses, and reports "yes" in the result.
    let body = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
printf '{"type":"control_request","request_id":"c1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"ls:*"}],"behavior":"allow","destination":"session"}],"blocked_path":null,"tool_use_id":"tu1","title":"Claude wants to run ls"}}\n'
read -r resp1
printf '{"type":"control_request","request_id":"c2","request":{"subtype":"mcp_message","server_name":"calc","message":{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}}}\n'
read -r resp2
ok="yes"
printf '%s' "$resp1" | grep -q '"behavior":"allow"' || ok="no-allow"
printf '%s' "$resp1" | grep -q '"command":"ls -la"' || ok="no-updated-input"
printf '%s' "$resp1" | grep -q '"updatedPermissions"' || ok="no-updated-permissions"
printf '%s' "$resp2" | grep -q '"serverInfo"' || ok="no-server-info"
printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s1","result":"%s"}\n' "$ok"
cat > /dev/null
"#;
    let (_dir, cli_path) = fake_cli(body);

    type SeenCall = (String, usize, Option<String>);
    let seen: Arc<Mutex<Vec<SeenCall>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let calc = create_sdk_mcp_server(
        "calc",
        "1.0.0",
        vec![tool("noop", "No-op", json!({}), |_args| async {
            Ok(CallToolResult::text("ok"))
        })],
    );
    let options = ClaudeAgentOptions {
        cli_path: Some(cli_path),
        mcp_servers: McpServers::Map(HashMap::from([(
            "calc".to_string(),
            McpServerConfig::Sdk(calc),
        )])),
        can_use_tool: Some(Arc::new(move |tool_name, _input, context| {
            sink.lock().unwrap().push((
                tool_name,
                context.suggestions.len(),
                context.title.clone(),
            ));
            let suggestions = context.suggestions.clone();
            Box::pin(async move {
                Ok(PermissionResult::Allow(PermissionResultAllow {
                    updated_input: Some(json!({"command": "ls -la"}).as_object().unwrap().clone()),
                    updated_permissions: Some(suggestions),
                }))
            })
        })),
        ..Default::default()
    };

    let result_text = run_and_get_result_text(options, "run ls").await;
    assert_eq!(result_text, "yes");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (tool_name, suggestion_count, title) = &seen[0];
    assert_eq!(tool_name, "Bash");
    assert_eq!(*suggestion_count, 1);
    assert_eq!(title.as_deref(), Some("Claude wants to run ls"));
}

#[tokio::test]
async fn hook_callback_round_trip() {
    // The fake CLI extracts the registered hook callback id from the
    // initialize request, invokes it, and checks the CLI-format field names
    // (continue, permissionDecision) in the response.
    let body = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
cb=$(printf '%s' "$line" | sed -n 's/.*"hookCallbackIds":\["\([^"]*\)"\].*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
printf '{"type":"control_request","request_id":"c1","request":{"subtype":"hook_callback","callback_id":"%s","input":{"hook_event_name":"PreToolUse","session_id":"s1","transcript_path":"/t","cwd":"/w","tool_name":"Bash","tool_input":{"command":"rm -rf /"},"tool_use_id":"tu1"},"tool_use_id":"tu1"}}\n' "$cb"
read -r resp
ok="yes"
printf '%s' "$resp" | grep -q '"continue":false' || ok="no-continue"
printf '%s' "$resp" | grep -q '"permissionDecision":"deny"' || ok="no-decision"
printf '%s' "$resp" | grep -q '"hookEventName":"PreToolUse"' || ok="no-event-name"
printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s1","result":"%s"}\n' "$ok"
cat > /dev/null
"#;
    let (_dir, cli_path) = fake_cli(body);

    let commands: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = commands.clone();
    let hook: claude_agent_sdk::HookCallback = Arc::new(move |input, tool_use_id, _context| {
        assert_eq!(tool_use_id.as_deref(), Some("tu1"));
        if let HookInput::PreToolUse(pre) = &input {
            sink.lock().unwrap().push(
                pre.tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            );
        } else {
            panic!("expected PreToolUse input, got {input:?}");
        }
        Box::pin(async move {
            Ok(HookJsonOutput::Sync(SyncHookJsonOutput {
                continue_: Some(false),
                hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                    permission_decision: Some(PermissionDecision::Deny),
                    permission_decision_reason: Some("blocked".to_string()),
                    updated_input: None,
                    additional_context: None,
                }),
                ..Default::default()
            }))
        })
    });

    let options = ClaudeAgentOptions {
        cli_path: Some(cli_path),
        hooks: Some(HashMap::from([(
            HookEvent::PreToolUse,
            vec![HookMatcher {
                matcher: Some("Bash".to_string()),
                hooks: vec![hook],
                timeout: None,
            }],
        )])),
        ..Default::default()
    };

    let result_text = run_and_get_result_text(options, "try something dangerous").await;
    assert_eq!(result_text, "yes");
    assert_eq!(*commands.lock().unwrap(), vec!["rm -rf /".to_string()]);
}

#[tokio::test]
async fn permission_suggestions_round_trip_shapes() {
    // The wire shape of suggestions parsed in ToolPermissionContext matches
    // PermissionUpdate's serde model.
    let wire = json!({
        "type": "addRules",
        "rules": [{"toolName": "Bash", "ruleContent": "ls:*"}],
        "behavior": "allow",
        "destination": "session",
    });
    let parsed: PermissionUpdate = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
}

/// An [`claude_agent_sdk::SdkMcpServer`] whose close() panics, to prove
/// shutdown survives user-code panics.
struct PanickyServer;

#[async_trait::async_trait]
impl claude_agent_sdk::SdkMcpServer for PanickyServer {
    async fn handle_message(
        &self,
        _message: serde_json::Value,
    ) -> claude_agent_sdk::Result<Option<serde_json::Value>> {
        Ok(None)
    }

    async fn close(&self) {
        panic!("user close() panicked");
    }
}

#[tokio::test]
async fn panicking_mcp_close_does_not_abort_shutdown() {
    // Regression test: a panic in a user-provided SdkMcpServer::close() must
    // not unwind out of disconnect() — the transport still has to close so
    // the subprocess is reaped.
    let body = r#"
echo $$ > "$PID_FILE"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
cat > /dev/null
"#;
    let (dir, cli_path) = fake_cli(body);
    let pid_file = dir.path().join("cli.pid");

    let options = ClaudeAgentOptions {
        cli_path: Some(cli_path),
        env: HashMap::from([(
            "PID_FILE".to_string(),
            pid_file.to_string_lossy().to_string(),
        )]),
        mcp_servers: McpServers::Map(HashMap::from([(
            "panicky".to_string(),
            McpServerConfig::Sdk(claude_agent_sdk::McpSdkServerConfig {
                name: "panicky".to_string(),
                instance: Arc::new(PanickyServer),
            }),
        )])),
        ..Default::default()
    };

    let mut client = claude_agent_sdk::ClaudeSdkClient::new(options);
    client.connect(None).await.expect("connect");

    let mut pid = None;
    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(parsed) = content.trim().parse::<i32>() {
                pid = Some(parsed);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pid = pid.expect("fake CLI pid");

    // Must not panic despite the server's close() panicking.
    client.disconnect().await.expect("disconnect");

    // And the subprocess must still have been shut down.
    let mut alive = true;
    for _ in 0..200 {
        alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !alive,
        "fake CLI (pid {pid}) should have exited after disconnect"
    );
}

#[tokio::test]
async fn panicking_callback_still_answers_control_request() {
    // Regression test (codex round-3 finding): a PANIC in a user callback
    // must not swallow the control_response — the CLI blocks until it gets
    // one, so the SDK answers with an error response instead.
    let body = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
printf '{"type":"control_request","request_id":"c1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"tu1"}}\n'
read -r resp
ok="yes"
printf '%s' "$resp" | grep -q '"subtype":"error"' || ok="no-error-subtype"
printf '%s' "$resp" | grep -q 'callback panicked' || ok="no-panic-text"
printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s1","result":"%s"}\n' "$ok"
cat > /dev/null
"#;
    let (_dir, cli_path) = fake_cli(body);
    let options = ClaudeAgentOptions {
        cli_path: Some(cli_path),
        can_use_tool: Some(Arc::new(|_tool_name, _input, _context| {
            Box::pin(async move { panic!("user callback exploded") })
        })),
        ..Default::default()
    };
    let result_text = run_and_get_result_text(options, "run ls").await;
    assert_eq!(result_text, "yes");
}
