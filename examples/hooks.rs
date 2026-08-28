//! Hooks example, ported from the Python SDK's `hooks.py`: a PreToolUse
//! hook that blocks dangerous Bash commands.
//!
//! Run with: `cargo run --example hooks`

use std::collections::HashMap;
use std::sync::Arc;

use claude_agent_sdk::{
    query, ClaudeAgentOptions, ContentBlock, HookEvent, HookInput, HookJsonOutput, HookMatcher,
    HookSpecificOutput, Message, PermissionDecision, SyncHookJsonOutput, ToolsConfig,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let check_bash: claude_agent_sdk::HookCallback =
        Arc::new(|input: HookInput, _tool_use_id, _context| {
            Box::pin(async move {
                if let HookInput::PreToolUse(pre) = &input {
                    let command = pre
                        .tool_input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if command.contains("rm -rf") {
                        println!("Hook blocked: {command}");
                        return Ok(HookJsonOutput::Sync(SyncHookJsonOutput {
                            hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                                permission_decision: Some(PermissionDecision::Deny),
                                permission_decision_reason: Some(
                                    "rm -rf is blocked by policy".to_string(),
                                ),
                                updated_input: None,
                                additional_context: None,
                            }),
                            ..Default::default()
                        }));
                    }
                }
                Ok(HookJsonOutput::default())
            })
                as futures::future::BoxFuture<'static, claude_agent_sdk::Result<HookJsonOutput>>
        });

    let options = ClaudeAgentOptions {
        tools: Some(ToolsConfig::List(vec!["Bash".into()])),
        hooks: Some(HashMap::from([(
            HookEvent::PreToolUse,
            vec![HookMatcher {
                matcher: Some("Bash".to_string()),
                hooks: vec![check_bash],
                timeout: None,
            }],
        )])),
        ..Default::default()
    };

    let mut messages = query("Run `echo safe` in bash.", options).await?;
    while let Some(message) = messages.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("Claude: {}", text.text);
                }
            }
        }
    }
    Ok(())
}
