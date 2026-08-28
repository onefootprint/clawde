//! End-to-end tests driving the SDK against a fake `claude` CLI script,
//! mirroring the Python SDK's transport/integration tests.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;

use claude_agent_sdk::{
    query, ClaudeAgentOptions, ClaudeSdkClient, ClaudeSdkError, ContentBlock, Message,
};
use futures::StreamExt;

/// Write an executable fake CLI script into a temp dir and return its path
/// (plus the tempdir guard keeping it alive).
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

/// A fake CLI that completes the initialize handshake, then answers the
/// first user message with one assistant message and a result.
const HAPPY_PATH: &str = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"commands":["/help"],"output_style":"default"}}}\n' "$id"
read -r user
printf '{"type":"assistant","message":{"role":"assistant","model":"claude-fake","content":[{"type":"text","text":"Hello from fake CLI!"}]},"session_id":"s1"}\n'
printf '{"type":"result","subtype":"success","duration_ms":42,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"s1","total_cost_usd":0.001,"result":"done"}\n'
cat > /dev/null
"#;

fn options_for(cli_path: &std::path::Path) -> ClaudeAgentOptions {
    ClaudeAgentOptions {
        cli_path: Some(cli_path.to_path_buf()),
        env: [(
            "CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK".to_string(),
            "1".to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn query_round_trip() {
    let (_dir, cli_path) = fake_cli(HAPPY_PATH);
    let mut messages = query("Hi there", options_for(&cli_path))
        .await
        .expect("query");

    let mut texts = Vec::new();
    let mut result = None;
    while let Some(message) = messages.next().await {
        match message.expect("message") {
            Message::Assistant(assistant) => {
                for block in assistant.content {
                    if let ContentBlock::Text(text) = block {
                        texts.push(text.text);
                    }
                }
            }
            Message::Result(r) => result = Some(r),
            _ => {}
        }
    }
    assert_eq!(texts, vec!["Hello from fake CLI!".to_string()]);
    let result = result.expect("result message");
    assert_eq!(result.session_id, "s1");
    assert_eq!(result.total_cost_usd, Some(0.001));
    assert_eq!(result.result.as_deref(), Some("done"));
}

#[tokio::test]
async fn client_round_trip_with_server_info() {
    let (_dir, cli_path) = fake_cli(HAPPY_PATH);
    let mut client = ClaudeSdkClient::new(options_for(&cli_path));
    client.connect(None).await.expect("connect");

    // The initialize handshake response is exposed as server info.
    let info = client
        .get_server_info()
        .await
        .expect("server info")
        .expect("some");
    assert_eq!(info["commands"][0], "/help");

    client.query("Hi", None).await.expect("send");
    let mut got_result = false;
    let mut responses = client.receive_response();
    while let Some(message) = responses.next().await {
        if let Message::Result(_) = message.expect("message") {
            got_result = true;
        }
    }
    drop(responses);
    assert!(got_result);
    client.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn error_result_becomes_result_error() {
    // A CLI that reports an error result and exits non-zero: the trailing
    // process error must be replaced by a ResultError carrying the payload.
    let body = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
printf '{"type":"result","subtype":"error_max_turns","duration_ms":1,"duration_api_ms":1,"is_error":true,"num_turns":9,"session_id":"s1","errors":["max turns reached"],"terminal_reason":"max_turns"}\n'
exit 1
"#;
    let (_dir, cli_path) = fake_cli(body);
    let mut messages = query("Hi", options_for(&cli_path)).await.expect("query");

    let mut saw_error_result = false;
    let mut trailing_error = None;
    while let Some(message) = messages.next().await {
        match message {
            Ok(Message::Result(result)) => {
                assert!(result.is_error);
                assert_eq!(
                    result.errors.as_deref(),
                    Some(&["max turns reached".to_string()][..])
                );
                saw_error_result = true;
            }
            Ok(_) => {}
            Err(e) => trailing_error = Some(e),
        }
    }
    assert!(saw_error_result);
    let error = trailing_error.expect("trailing error");
    let ClaudeSdkError::ResultError { .. } = &error else {
        panic!("expected ResultError, got {error:?}");
    };
    assert_eq!(error.subtype(), Some("error_max_turns"));
    assert_eq!(error.terminal_reason(), Some("max_turns"));
    assert_eq!(error.errors(), vec!["max turns reached".to_string()]);
    assert_eq!(error.exit_code(), Some(1));
    assert!(error.to_string().contains("max turns reached"));
}

#[tokio::test]
async fn non_json_stdout_lines_are_skipped() {
    let body = r#"
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
echo '[SandboxDebug] noise on stdout'
echo ''
printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s1"}\n'
cat > /dev/null
"#;
    let (_dir, cli_path) = fake_cli(body);
    let mut messages = query("Hi", options_for(&cli_path)).await.expect("query");
    let mut count = 0;
    while let Some(message) = messages.next().await {
        message.expect("message");
        count += 1;
    }
    // Only the result message survives; noise lines are dropped.
    assert_eq!(count, 1);
}

#[tokio::test]
async fn stderr_callback_receives_lines() {
    let body = r#"
echo 'debug line one' >&2
printf 'partial tail' >&2
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
read -r user
printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s1"}\n'
cat > /dev/null
"#;
    let (_dir, cli_path) = fake_cli(body);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = lines.clone();
    let mut options = options_for(&cli_path);
    options.stderr = Some(std::sync::Arc::new(move |line: &str| {
        sink.lock().unwrap().push(line.to_string());
    }));

    let mut messages = query("Hi", options).await.expect("query");
    while let Some(message) = messages.next().await {
        message.expect("message");
    }
    // Give the stderr task a moment to flush its tail.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let lines = lines.lock().unwrap().clone();
    assert!(lines.contains(&"debug line one".to_string()), "{lines:?}");
    assert!(lines.contains(&"partial tail".to_string()), "{lines:?}");
}

#[tokio::test]
async fn missing_cli_fails_with_not_found() {
    let options = ClaudeAgentOptions {
        cli_path: Some(PathBuf::from("/nonexistent/claude")),
        ..Default::default()
    };
    let error = query("Hi", options).await.err().expect("spawn failure");
    assert!(matches!(
        error,
        ClaudeSdkError::CliNotFound { .. } | ClaudeSdkError::CliConnection { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_stream_off_runtime_still_kills_subprocess() {
    // Regression test: MessageStream::drop must clean up (and terminate the
    // CLI subprocess) even when the drop happens on a non-runtime thread,
    // via the runtime handle captured at construction.
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("cli.pid");
    let body = format!(
        r#"
echo $$ > {pid}
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{}}}}}}\n' "$id"
cat > /dev/null
"#,
        pid = pid_file.display()
    );
    let (_cli_dir, cli_path) = fake_cli(&body);

    let messages = query("Hi", options_for(&cli_path)).await.expect("query");
    // Wait until the fake CLI has written its pid.
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

    // Drop the stream on a plain OS thread — no tokio context there.
    std::thread::spawn(move || drop(messages))
        .join()
        .expect("drop thread");

    // The spawned cleanup should close stdin and reap the process.
    let mut alive = true;
    for _ in 0..200 {
        alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!alive, "fake CLI (pid {pid}) should have exited after drop");
}
