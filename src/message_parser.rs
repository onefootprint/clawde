//! Message parser for Claude Code CLI responses.

use serde_json::{Map, Value};

use crate::errors::{ClaudeSdkError, Result};
use crate::types::*;

fn parse_error(message: impl Into<String>, data: &Value) -> ClaudeSdkError {
    ClaudeSdkError::MessageParse {
        message: message.into(),
        data: Some(data.clone()),
    }
}

fn required_str(data: &Value, obj: &Value, key: &str, context: &str) -> Result<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            parse_error(
                format!("Missing required field in {context}: '{key}'"),
                data,
            )
        })
}

fn required_i64(data: &Value, obj: &Value, key: &str, context: &str) -> Result<i64> {
    obj.get(key).and_then(Value::as_i64).ok_or_else(|| {
        parse_error(
            format!("Missing required field in {context}: '{key}'"),
            data,
        )
    })
}

fn optional_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Return `data["origin"]` if it is a well-formed origin object.
///
/// Passed through as-is (including keys this SDK version doesn't model) so
/// newer CLI origin kinds/fields stay visible to callers. Anything that is
/// not an object with a string `kind` is treated as absent.
fn parse_origin(data: &Value) -> Option<MessageOrigin> {
    let origin = data.get("origin")?;
    if origin.get("kind").is_some_and(Value::is_string) {
        serde_json::from_value(origin.clone()).ok()
    } else {
        None
    }
}

fn parse_user_content_block(block: &Value, data: &Value) -> Result<Option<ContentBlock>> {
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| parse_error("Invalid content block (missing 'type')", data))?;
    let context = "user message";
    Ok(match block_type {
        "text" => Some(ContentBlock::Text(TextBlock {
            text: required_str(data, block, "text", context)?,
        })),
        "tool_use" => Some(ContentBlock::ToolUse(ToolUseBlock {
            id: required_str(data, block, "id", context)?,
            name: required_str(data, block, "name", context)?,
            input: block.get("input").cloned().unwrap_or(Value::Null),
        })),
        "tool_result" => Some(ContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: required_str(data, block, "tool_use_id", context)?,
            content: block.get("content").cloned(),
            is_error: block.get("is_error").and_then(Value::as_bool),
        })),
        _ => None,
    })
}

fn parse_assistant_content_block(block: &Value, data: &Value) -> Result<Option<ContentBlock>> {
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| parse_error("Invalid content block (missing 'type')", data))?;
    let context = "assistant message";
    Ok(match block_type {
        "text" => Some(ContentBlock::Text(TextBlock {
            text: required_str(data, block, "text", context)?,
        })),
        "thinking" => Some(ContentBlock::Thinking(ThinkingBlock {
            thinking: required_str(data, block, "thinking", context)?,
            signature: required_str(data, block, "signature", context)?,
        })),
        "tool_use" => Some(ContentBlock::ToolUse(ToolUseBlock {
            id: required_str(data, block, "id", context)?,
            name: required_str(data, block, "name", context)?,
            input: block.get("input").cloned().unwrap_or(Value::Null),
        })),
        "tool_result" => Some(ContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: required_str(data, block, "tool_use_id", context)?,
            content: block.get("content").cloned(),
            is_error: block.get("is_error").and_then(Value::as_bool),
        })),
        "server_tool_use" => Some(ContentBlock::ServerToolUse(ServerToolUseBlock {
            id: required_str(data, block, "id", context)?,
            name: serde_json::from_value(block.get("name").cloned().unwrap_or(Value::Null))
                .map_err(|_| {
                    parse_error("Missing required field in assistant message: 'name'", data)
                })?,
            input: block.get("input").cloned().unwrap_or(Value::Null),
        })),
        "advisor_tool_result" => Some(ContentBlock::ServerToolResult(ServerToolResultBlock {
            tool_use_id: required_str(data, block, "tool_use_id", context)?,
            content: block.get("content").cloned().ok_or_else(|| {
                parse_error(
                    "Missing required field in assistant message: 'content'",
                    data,
                )
            })?,
        })),
        _ => None,
    })
}

fn parse_task_usage(value: Option<&Value>) -> Option<TaskUsage> {
    value.and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn parse_system_message(data: &Value, subtype: &str) -> Result<Message> {
    let context = "system message";
    match subtype {
        "task_started" => Ok(Message::TaskStarted(TaskStartedMessage {
            subtype: subtype.to_string(),
            data: data.clone(),
            task_id: required_str(data, data, "task_id", context)?,
            description: required_str(data, data, "description", context)?,
            uuid: required_str(data, data, "uuid", context)?,
            session_id: required_str(data, data, "session_id", context)?,
            tool_use_id: optional_str(data, "tool_use_id"),
            task_type: optional_str(data, "task_type"),
        })),
        "task_progress" => Ok(Message::TaskProgress(TaskProgressMessage {
            subtype: subtype.to_string(),
            data: data.clone(),
            task_id: required_str(data, data, "task_id", context)?,
            description: required_str(data, data, "description", context)?,
            usage: parse_task_usage(data.get("usage")).ok_or_else(|| {
                parse_error("Missing required field in system message: 'usage'", data)
            })?,
            uuid: required_str(data, data, "uuid", context)?,
            session_id: required_str(data, data, "session_id", context)?,
            tool_use_id: optional_str(data, "tool_use_id"),
            last_tool_name: optional_str(data, "last_tool_name"),
        })),
        "task_notification" => Ok(Message::TaskNotification(TaskNotificationMessage {
            subtype: subtype.to_string(),
            data: data.clone(),
            task_id: required_str(data, data, "task_id", context)?,
            status: serde_json::from_value(data.get("status").cloned().unwrap_or(Value::Null))
                .map_err(|_| {
                    parse_error("Missing required field in system message: 'status'", data)
                })?,
            output_file: required_str(data, data, "output_file", context)?,
            summary: required_str(data, data, "summary", context)?,
            uuid: required_str(data, data, "uuid", context)?,
            session_id: required_str(data, data, "session_id", context)?,
            tool_use_id: optional_str(data, "tool_use_id"),
            usage: parse_task_usage(data.get("usage")),
        })),
        "task_updated" => {
            // Terminal task completion sometimes arrives only as a
            // task_updated patch (no separate task_notification), so expose
            // it as a typed lifecycle message rather than a generic
            // SystemMessage. Parsed defensively: the patch may omit
            // uuid/session_id and parsing must never fail on a lifecycle
            // event.
            let patch: Map<String, Value> = data
                .get("patch")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Terminal-ness is derived from patch.status; a patch that
            // carries only end_time/result/error (no status) is left
            // non-terminal (status=None) — the full patch is still preserved
            // on .patch for callers that need more.
            let status = patch
                .get("status")
                .and_then(|s| serde_json::from_value(s.clone()).ok());
            Ok(Message::TaskUpdated(TaskUpdatedMessage {
                subtype: subtype.to_string(),
                data: data.clone(),
                task_id: optional_str(data, "task_id").unwrap_or_default(),
                patch,
                status,
                session_id: optional_str(data, "session_id"),
                uuid: optional_str(data, "uuid"),
            }))
        }
        // SDK-synthesized via report_mirror_error — never emitted by the CLI
        // subprocess.
        "mirror_error" => Ok(Message::MirrorError(MirrorErrorMessage {
            subtype: subtype.to_string(),
            data: data.clone(),
            key: data
                .get("key")
                .and_then(|k| serde_json::from_value(k.clone()).ok()),
            error: optional_str(data, "error").unwrap_or_default(),
        })),
        _ => Ok(Message::System(SystemMessage {
            subtype: subtype.to_string(),
            data: data.clone(),
        })),
    }
}

/// Parse a message from CLI output into a typed [`Message`].
///
/// Returns `Ok(None)` for unrecognized message types so newer CLI versions
/// don't break older SDK versions, and an error when a recognized message is
/// malformed.
pub fn parse_message(data: &Value) -> Result<Option<Message>> {
    if !data.is_object() {
        return Err(ClaudeSdkError::MessageParse {
            message: "Invalid message data type (expected object)".to_string(),
            data: Some(data.clone()),
        });
    }

    // Hook events (emitted when `include_hook_events` is enabled) arrive as
    // `system` messages with `subtype` of `hook_started` or `hook_response`.
    // Route them to `HookEventMessage` before the generic system handling
    // below.
    let msg_type = data.get("type").and_then(Value::as_str);
    let subtype = data.get("subtype").and_then(Value::as_str);
    if msg_type == Some("system") && matches!(subtype, Some("hook_started" | "hook_response")) {
        let hook_event_name = data
            .get("hook_event")
            .or_else(|| data.get("hook_name"))
            .or_else(|| data.get("hook_event_name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Some(Message::HookEvent(HookEventMessage {
            subtype: subtype.unwrap_or_default().to_string(),
            hook_event_name,
            data: data.clone(),
            session_id: optional_str(data, "session_id"),
            uuid: optional_str(data, "uuid"),
        })));
    }

    let Some(message_type) = msg_type.filter(|t| !t.is_empty()) else {
        return Err(parse_error("Message missing 'type' field", data));
    };

    match message_type {
        "user" => {
            let message = data.get("message").ok_or_else(|| {
                parse_error("Missing required field in user message: 'message'", data)
            })?;
            let raw_content = message.get("content").ok_or_else(|| {
                parse_error("Missing required field in user message: 'content'", data)
            })?;
            let content = if let Some(blocks) = raw_content.as_array() {
                let mut user_content_blocks = Vec::new();
                for block in blocks {
                    if !block.is_object() {
                        return Err(parse_error("Invalid content block (expected object)", data));
                    }
                    if let Some(parsed) = parse_user_content_block(block, data)? {
                        user_content_blocks.push(parsed);
                    }
                }
                UserContent::Blocks(user_content_blocks)
            } else {
                UserContent::Text(raw_content.as_str().unwrap_or_default().to_string())
            };
            Ok(Some(Message::User(UserMessage {
                content,
                uuid: optional_str(data, "uuid"),
                parent_tool_use_id: optional_str(data, "parent_tool_use_id"),
                tool_use_result: data
                    .get("tool_use_result")
                    .cloned()
                    .filter(|v| !v.is_null()),
                origin: parse_origin(data),
            })))
        }

        "assistant" => {
            let message = data.get("message").ok_or_else(|| {
                parse_error(
                    "Missing required field in assistant message: 'message'",
                    data,
                )
            })?;
            let raw_content = message.get("content").ok_or_else(|| {
                parse_error(
                    "Missing required field in assistant message: 'content'",
                    data,
                )
            })?;
            let blocks = raw_content
                .as_array()
                .ok_or_else(|| parse_error("Invalid assistant content (expected list)", data))?;
            let mut content_blocks = Vec::new();
            for block in blocks {
                if !block.is_object() {
                    return Err(parse_error("Invalid content block (expected object)", data));
                }
                if let Some(parsed) = parse_assistant_content_block(block, data)? {
                    content_blocks.push(parsed);
                }
            }
            Ok(Some(Message::Assistant(AssistantMessage {
                content: content_blocks,
                model: required_str(data, message, "model", "assistant message")?,
                parent_tool_use_id: optional_str(data, "parent_tool_use_id"),
                error: data
                    .get("error")
                    .and_then(|e| serde_json::from_value(e.clone()).ok()),
                usage: message.get("usage").cloned().filter(|v| !v.is_null()),
                message_id: optional_str(message, "id"),
                stop_reason: optional_str(message, "stop_reason"),
                session_id: optional_str(data, "session_id"),
                uuid: optional_str(data, "uuid"),
            })))
        }

        "system" => {
            let subtype = subtype.ok_or_else(|| {
                parse_error("Missing required field in system message: 'subtype'", data)
            })?;
            parse_system_message(data, subtype).map(Some)
        }

        "result" => {
            let context = "result message";
            let deferred = data.get("deferred_tool_use").filter(|v| !v.is_null());
            Ok(Some(Message::Result(Box::new(ResultMessage {
                subtype: required_str(data, data, "subtype", context)?,
                duration_ms: required_i64(data, data, "duration_ms", context)?,
                duration_api_ms: required_i64(data, data, "duration_api_ms", context)?,
                is_error: data
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        parse_error("Missing required field in result message: 'is_error'", data)
                    })?,
                num_turns: required_i64(data, data, "num_turns", context)?,
                session_id: required_str(data, data, "session_id", context)?,
                stop_reason: optional_str(data, "stop_reason"),
                total_cost_usd: data.get("total_cost_usd").and_then(Value::as_f64),
                usage: data.get("usage").cloned().filter(|v| !v.is_null()),
                result: optional_str(data, "result"),
                structured_output: data
                    .get("structured_output")
                    .cloned()
                    .filter(|v| !v.is_null()),
                model_usage: data
                    .get("modelUsage")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                permission_denials: data
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .cloned(),
                deferred_tool_use: match deferred {
                    Some(d) => Some(DeferredToolUse {
                        id: required_str(data, d, "id", context)?,
                        name: required_str(data, d, "name", context)?,
                        input: d.get("input").cloned().unwrap_or(Value::Null),
                    }),
                    None => None,
                },
                errors: data
                    .get("errors")
                    .map(|raw| crate::errors::normalize_result_errors(Some(raw))),
                api_error_status: data.get("api_error_status").and_then(Value::as_i64),
                uuid: optional_str(data, "uuid"),
                terminal_reason: optional_str(data, "terminal_reason"),
                origin: parse_origin(data),
            }))))
        }

        "stream_event" => {
            let context = "stream_event message";
            Ok(Some(Message::Stream(StreamEvent {
                uuid: required_str(data, data, "uuid", context)?,
                session_id: required_str(data, data, "session_id", context)?,
                event: data.get("event").cloned().ok_or_else(|| {
                    parse_error(
                        "Missing required field in stream_event message: 'event'",
                        data,
                    )
                })?,
                parent_tool_use_id: optional_str(data, "parent_tool_use_id"),
            })))
        }

        "rate_limit_event" => {
            let context = "rate_limit_event message";
            let info = data.get("rate_limit_info").ok_or_else(|| {
                parse_error(
                    "Missing required field in rate_limit_event message: 'rate_limit_info'",
                    data,
                )
            })?;
            Ok(Some(Message::RateLimit(RateLimitEvent {
                rate_limit_info: RateLimitInfo {
                    status: serde_json::from_value(
                        info.get("status").cloned().unwrap_or(Value::Null),
                    )
                    .map_err(|_| {
                        parse_error(
                            "Missing required field in rate_limit_event message: 'status'",
                            data,
                        )
                    })?,
                    resets_at: info.get("resetsAt").and_then(Value::as_i64),
                    rate_limit_type: info
                        .get("rateLimitType")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    utilization: info.get("utilization").and_then(Value::as_f64),
                    overage_status: info
                        .get("overageStatus")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    overage_resets_at: info.get("overageResetsAt").and_then(Value::as_i64),
                    overage_disabled_reason: optional_str(info, "overageDisabledReason"),
                    raw: info.clone(),
                },
                uuid: required_str(data, data, "uuid", context)?,
                session_id: required_str(data, data, "session_id", context)?,
            })))
        }

        "conversation_reset" => {
            let context = "conversation_reset message";
            Ok(Some(Message::ConversationReset(ConversationResetMessage {
                new_conversation_id: required_str(data, data, "new_conversation_id", context)?,
                uuid: required_str(data, data, "uuid", context)?,
                session_id: required_str(data, data, "session_id", context)?,
            })))
        }

        // Forward-compatible: skip unrecognized message types so newer CLI
        // versions don't crash older SDK versions.
        other => {
            tracing::debug!(target: "claude_agent_sdk", "Skipping unknown message type: {other}");
            Ok(None)
        }
    }
}
