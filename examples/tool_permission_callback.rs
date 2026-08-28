//! Tool permission callback example, ported from the Python SDK's
//! `tool_permission_callback.py`.
//!
//! Run with: `cargo run --example tool_permission_callback`

use std::sync::Arc;

use claude_agent_sdk::{
    query, ClaudeAgentOptions, ContentBlock, Message, PermissionResult, ToolsConfig,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions {
        tools: Some(ToolsConfig::List(vec!["Bash".into(), "Read".into()])),
        can_use_tool: Some(Arc::new(|tool_name, input, context| {
            Box::pin(async move {
                println!("Permission requested for {tool_name}");
                if let Some(title) = &context.title {
                    println!("  {title}");
                }
                // Deny anything that looks destructive; allow the rest.
                let command = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if command.contains("rm ") {
                    Ok(PermissionResult::deny(
                        "Destructive commands are not allowed",
                    ))
                } else {
                    Ok(PermissionResult::allow())
                }
            })
        })),
        ..Default::default()
    };

    let mut messages = query("Run `echo hello` and show me the output.", options).await?;
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
