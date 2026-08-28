//! Minimal live smoke test against the installed Claude Code CLI.
//!
//! Run with: `cargo run --example smoke`

use clawde::{query, ClaudeAgentOptions, ContentBlock, Message, ToolsConfig};
use futures::StreamExt;

#[tokio::main]
async fn main() -> clawde::Result<()> {
    let options = ClaudeAgentOptions {
        tools: Some(ToolsConfig::List(vec![])),
        max_turns: Some(1),
        ..Default::default()
    };
    let mut messages = query("Reply with exactly the word: pong", options).await?;
    while let Some(message) = messages.next().await {
        match message? {
            Message::Assistant(assistant) => {
                for block in assistant.content {
                    if let ContentBlock::Text(text) = block {
                        println!("assistant: {}", text.text);
                    }
                }
            }
            Message::Result(result) => {
                println!(
                    "result: subtype={} is_error={} turns={} cost={:?}",
                    result.subtype, result.is_error, result.num_turns, result.total_cost_usd
                );
            }
            other => println!("other: {other:?}"),
        }
    }
    Ok(())
}
