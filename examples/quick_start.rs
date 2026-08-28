//! Quick start example, ported from the Python SDK's `quick_start.py`.
//!
//! Run with: `cargo run --example quick_start`

use clawde::{query, ClaudeAgentOptions, ContentBlock, Message, ToolsConfig};
use futures::StreamExt;

async fn basic_example() -> clawde::Result<()> {
    println!("=== Basic Example ===");
    let mut messages = query("What is 2 + 2?", ClaudeAgentOptions::default()).await?;
    while let Some(message) = messages.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("Claude: {}", text.text);
                }
            }
        }
    }
    println!();
    Ok(())
}

async fn with_options_example() -> clawde::Result<()> {
    println!("=== With Options Example ===");
    let options = ClaudeAgentOptions {
        system_prompt: Some("You are a helpful assistant that explains things simply.".into()),
        max_turns: Some(1),
        ..Default::default()
    };
    let mut messages = query("Explain what Rust is in one sentence.", options).await?;
    while let Some(message) = messages.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("Claude: {}", text.text);
                }
            }
        }
    }
    println!();
    Ok(())
}

async fn with_tools_example() -> clawde::Result<()> {
    println!("=== With Tools Example ===");
    let options = ClaudeAgentOptions {
        tools: Some(ToolsConfig::List(vec!["Read".into(), "Write".into()])),
        system_prompt: Some("You are a helpful file assistant.".into()),
        ..Default::default()
    };
    let mut messages = query(
        "Create a file called hello.txt with 'Hello, World!' in it",
        options,
    )
    .await?;
    while let Some(message) = messages.next().await {
        match message? {
            Message::Assistant(assistant) => {
                for block in assistant.content {
                    if let ContentBlock::Text(text) = block {
                        println!("Claude: {}", text.text);
                    }
                }
            }
            Message::Result(result) => {
                if let Some(cost) = result.total_cost_usd {
                    if cost > 0.0 {
                        println!("\nCost: ${cost:.4}");
                    }
                }
            }
            _ => {}
        }
    }
    println!();
    Ok(())
}

#[tokio::main]
async fn main() -> clawde::Result<()> {
    basic_example().await?;
    with_options_example().await?;
    with_tools_example().await?;
    Ok(())
}
