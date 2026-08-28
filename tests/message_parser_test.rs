//! Tests for message parsing, ported from the Python SDK's
//! `test_message_parser.py`.

use claude_agent_sdk::{
    parse_message, ClaudeSdkError, ContentBlock, Message, MessageOriginKind, ServerToolName,
    TaskNotificationStatus, TaskUpdatedStatus, UserContent,
};
use serde_json::json;

#[test]
fn parses_simple_user_message() {
    let data = json!({
        "type": "user",
        "message": {"role": "user", "content": "Hello Claude"},
    });
    let message = parse_message(&data).unwrap().unwrap();
    let Message::User(user) = message else {
        panic!("expected user message");
    };
    assert_eq!(user.content, UserContent::Text("Hello Claude".to_string()));
    assert!(user.uuid.is_none());
    assert!(user.origin.is_none());
}

#[test]
fn parses_user_message_with_blocks_and_origin() {
    let data = json!({
        "type": "user",
        "uuid": "u-1",
        "parent_tool_use_id": "tu-1",
        "origin": {"kind": "task-notification", "subkind": "scheduled-trigger", "newField": 1},
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_result", "tool_use_id": "tu-0", "content": "ok", "is_error": false},
                {"type": "tool_use", "id": "tu-2", "name": "Bash", "input": {"command": "ls"}},
                {"type": "unknown_block", "x": 1},
            ],
        },
    });
    let Message::User(user) = parse_message(&data).unwrap().unwrap() else {
        panic!("expected user message");
    };
    let UserContent::Blocks(blocks) = user.content else {
        panic!("expected block content");
    };
    // Unknown block types are skipped.
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hi"));
    assert!(matches!(&blocks[1], ContentBlock::ToolResult(r) if r.is_error == Some(false)));
    assert!(matches!(&blocks[2], ContentBlock::ToolUse(u) if u.name == "Bash"));
    let origin = user.origin.expect("origin");
    assert_eq!(origin.kind, MessageOriginKind::TaskNotification);
    assert!(!origin.is_human());
    assert_eq!(origin.extra.get("newField"), Some(&json!(1)));
}

#[test]
fn parses_assistant_message_with_all_block_types() {
    let data = json!({
        "type": "assistant",
        "session_id": "s1",
        "message": {
            "model": "claude-opus-4-1",
            "id": "msg_1",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10},
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/x"}},
                {"type": "server_tool_use", "id": "st1", "name": "web_search", "input": {"query": "q"}},
                {"type": "advisor_tool_result", "tool_use_id": "st0", "content": {"type": "advisor_result"}},
                {"type": "brand_new_block"},
            ],
        },
    });
    let Message::Assistant(assistant) = parse_message(&data).unwrap().unwrap() else {
        panic!("expected assistant message");
    };
    assert_eq!(assistant.model, "claude-opus-4-1");
    assert_eq!(assistant.message_id.as_deref(), Some("msg_1"));
    assert_eq!(assistant.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(assistant.content.len(), 5);
    assert!(matches!(
        &assistant.content[3],
        ContentBlock::ServerToolUse(s) if s.name == ServerToolName::WebSearch
    ));
    assert!(matches!(
        &assistant.content[4],
        ContentBlock::ServerToolResult(r) if r.tool_use_id == "st0"
    ));
}

#[test]
fn parses_result_message() {
    let data = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 800,
        "is_error": false,
        "num_turns": 2,
        "session_id": "s1",
        "total_cost_usd": 0.01,
        "modelUsage": {
            "claude-opus-4-1": {
                "inputTokens": 100, "outputTokens": 5, "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0, "webSearchRequests": 0,
                "costUSD": 0.01, "contextWindow": 200000, "maxOutputTokens": 32000,
                "provider": "firstParty"
            }
        },
        "terminal_reason": "completed",
    });
    let Message::Result(result) = parse_message(&data).unwrap().unwrap() else {
        panic!("expected result message");
    };
    assert_eq!(result.subtype, "success");
    assert!(!result.is_error);
    assert_eq!(result.total_cost_usd, Some(0.01));
    assert_eq!(result.terminal_reason.as_deref(), Some("completed"));
    let usage = &result.model_usage.as_ref().unwrap()["claude-opus-4-1"];
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.provider.as_deref(), Some("firstParty"));
}

#[test]
fn parses_result_message_with_deferred_tool_use() {
    let data = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1, "duration_api_ms": 1, "is_error": false,
        "num_turns": 1, "session_id": "s1",
        "deferred_tool_use": {"id": "d1", "name": "Bash", "input": {"command": "rm -rf /"}},
    });
    let Message::Result(result) = parse_message(&data).unwrap().unwrap() else {
        panic!("expected result message");
    };
    let deferred = result.deferred_tool_use.expect("deferred tool use");
    assert_eq!(deferred.name, "Bash");
}

#[test]
fn parses_system_and_task_messages() {
    let generic = json!({"type": "system", "subtype": "init", "session_id": "s1"});
    assert!(matches!(
        parse_message(&generic).unwrap().unwrap(),
        Message::System(system) if system.subtype == "init"
    ));

    let started = json!({
        "type": "system", "subtype": "task_started",
        "task_id": "t1", "description": "do work", "uuid": "u1",
        "session_id": "s1", "task_type": "local_agent",
    });
    assert!(matches!(
        parse_message(&started).unwrap().unwrap(),
        Message::TaskStarted(m) if m.task_id == "t1" && m.task_type.as_deref() == Some("local_agent")
    ));

    let progress = json!({
        "type": "system", "subtype": "task_progress",
        "task_id": "t1", "description": "working",
        "usage": {"total_tokens": 5, "tool_uses": 1, "duration_ms": 100},
        "uuid": "u2", "session_id": "s1",
    });
    assert!(matches!(
        parse_message(&progress).unwrap().unwrap(),
        Message::TaskProgress(m) if m.usage.total_tokens == 5
    ));

    let notification = json!({
        "type": "system", "subtype": "task_notification",
        "task_id": "t1", "status": "completed", "output_file": "/tmp/out",
        "summary": "done", "uuid": "u3", "session_id": "s1",
    });
    assert!(matches!(
        parse_message(&notification).unwrap().unwrap(),
        Message::TaskNotification(m) if m.status == TaskNotificationStatus::Completed
    ));

    // task_updated is parsed defensively: missing uuid/session_id must not
    // fail, and terminal-ness comes from patch.status.
    let updated = json!({
        "type": "system", "subtype": "task_updated",
        "task_id": "t1", "patch": {"status": "killed", "end_time": 5},
    });
    let Message::TaskUpdated(updated) = parse_message(&updated).unwrap().unwrap() else {
        panic!("expected task updated");
    };
    assert_eq!(updated.status, Some(TaskUpdatedStatus::Killed));
    assert_eq!(updated.patch.get("end_time"), Some(&json!(5)));
}

#[test]
fn routes_hook_events_before_generic_system() {
    let data = json!({
        "type": "system", "subtype": "hook_started",
        "hook_event": "PreToolUse", "session_id": "s1", "uuid": "u1",
    });
    assert!(matches!(
        parse_message(&data).unwrap().unwrap(),
        Message::HookEvent(m) if m.hook_event_name == "PreToolUse" && m.subtype == "hook_started"
    ));
}

#[test]
fn parses_stream_rate_limit_and_reset_messages() {
    let stream = json!({
        "type": "stream_event", "uuid": "u1", "session_id": "s1",
        "event": {"type": "content_block_delta"},
    });
    assert!(matches!(
        parse_message(&stream).unwrap().unwrap(),
        Message::Stream(e) if e.uuid == "u1"
    ));

    let rate = json!({
        "type": "rate_limit_event", "uuid": "u1", "session_id": "s1",
        "rate_limit_info": {
            "status": "allowed_warning", "resetsAt": 123, "rateLimitType": "five_hour",
            "utilization": 0.9,
        },
    });
    let Message::RateLimit(event) = parse_message(&rate).unwrap().unwrap() else {
        panic!("expected rate limit event");
    };
    assert_eq!(
        event.rate_limit_info.status,
        claude_agent_sdk::RateLimitStatus::AllowedWarning
    );
    assert_eq!(event.rate_limit_info.resets_at, Some(123));
    assert_eq!(
        event.rate_limit_info.rate_limit_type,
        Some(claude_agent_sdk::RateLimitType::FiveHour)
    );

    let reset = json!({
        "type": "conversation_reset", "new_conversation_id": "c2",
        "uuid": "u9", "session_id": "s1",
    });
    assert!(matches!(
        parse_message(&reset).unwrap().unwrap(),
        Message::ConversationReset(m) if m.new_conversation_id == "c2"
    ));
}

#[test]
fn skips_unknown_message_types() {
    let data = json!({"type": "brand_new_message_type", "anything": true});
    assert!(parse_message(&data).unwrap().is_none());
}

#[test]
fn errors_on_malformed_messages() {
    let missing_type = json!({"message": {"content": "hi"}});
    assert!(matches!(
        parse_message(&missing_type),
        Err(ClaudeSdkError::MessageParse { .. })
    ));

    let missing_fields = json!({"type": "result", "subtype": "success"});
    assert!(matches!(
        parse_message(&missing_fields),
        Err(ClaudeSdkError::MessageParse { .. })
    ));

    let not_object = json!("just a string");
    assert!(parse_message(&not_object).is_err());
}

#[test]
fn ignores_malformed_origin() {
    let data = json!({
        "type": "user",
        "origin": {"noKind": true},
        "message": {"role": "user", "content": "hello"},
    });
    let Message::User(user) = parse_message(&data).unwrap().unwrap() else {
        panic!("expected user message");
    };
    assert!(user.origin.is_none());
}
