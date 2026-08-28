//! In-process MCP server example, ported from the Python SDK's
//! `mcp_calculator.py`.
//!
//! Run with: `cargo run --example mcp_calculator`

use std::collections::HashMap;

use clawde::mcp::{create_sdk_mcp_server, tool, CallToolResult};
use clawde::{query, ClaudeAgentOptions, ContentBlock, McpServerConfig, McpServers, Message};
use futures::StreamExt;
use serde_json::json;

#[tokio::main]
async fn main() -> clawde::Result<()> {
    let number_schema = json!({"a": {"type": "number"}, "b": {"type": "number"}});

    let add = tool(
        "add",
        "Add two numbers",
        number_schema.clone(),
        |args| async move {
            let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
            Ok(CallToolResult::text(format!("The sum is {sum}")))
        },
    );

    let divide = tool(
        "divide",
        "Divide two numbers",
        number_schema,
        |args| async move {
            let b = args["b"].as_f64().unwrap_or(0.0);
            if b == 0.0 {
                return Ok(CallToolResult::error("Error: Division by zero"));
            }
            let quotient = args["a"].as_f64().unwrap_or(0.0) / b;
            Ok(CallToolResult::text(format!("The quotient is {quotient}")))
        },
    );

    let calculator = create_sdk_mcp_server("calculator", "1.0.0", vec![add, divide]);

    let options = ClaudeAgentOptions {
        mcp_servers: McpServers::Map(HashMap::from([(
            "calc".to_string(),
            McpServerConfig::Sdk(calculator),
        )])),
        allowed_tools: vec![
            "mcp__calc__add".to_string(),
            "mcp__calc__divide".to_string(),
        ],
        ..Default::default()
    };

    let mut messages = query("Use the calculator to compute 127 divided by 4.", options).await?;
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
