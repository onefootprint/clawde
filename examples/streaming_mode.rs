//! Interactive client example, ported from the Python SDK's
//! `streaming_mode.py`.
//!
//! Run with: `cargo run --example streaming_mode`

use claude_agent_sdk::{ClaudeAgentOptions, ClaudeSdkClient, ContentBlock, Message};
use futures::StreamExt;

fn display(message: &Message) {
    match message {
        Message::User(user) => {
            if let claude_agent_sdk::UserContent::Blocks(blocks) = &user.content {
                for block in blocks {
                    if let ContentBlock::ToolResult(result) = block {
                        println!("Tool result for {}", result.tool_use_id);
                    }
                }
            }
        }
        Message::Assistant(assistant) => {
            for block in &assistant.content {
                match block {
                    ContentBlock::Text(text) => println!("Claude: {}", text.text),
                    ContentBlock::ToolUse(tool_use) => {
                        println!("Using tool: {}", tool_use.name)
                    }
                    _ => {}
                }
            }
        }
        Message::Result(result) => {
            println!(
                "Result ended turn {} (cost: {:?})",
                result.num_turns, result.total_cost_usd
            );
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut client = ClaudeSdkClient::new(ClaudeAgentOptions::default());
    client.connect(None).await?;

    // First turn.
    client.query("What is 25 * 4?", None).await?;
    let mut responses = client.receive_response();
    while let Some(message) = responses.next().await {
        display(&message?);
    }
    drop(responses);

    // Follow-up in the same conversation.
    client.query("Now add 100 to that result.", None).await?;
    let mut responses = client.receive_response();
    while let Some(message) = responses.next().await {
        display(&message?);
    }
    drop(responses);

    client.disconnect().await
}
