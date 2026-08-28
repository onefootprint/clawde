//! # Clawde — the Claude Agent SDK for Rust
//!
//! A Rust port of the [Claude Agent SDK for Python][python-sdk]:
//! programmatic access to the Claude Code CLI, with a 1:1 public interface
//! expressed idiomatically in Rust.
//!
//! [python-sdk]: https://github.com/anthropics/claude-agent-sdk-python
//!
//! ## One-shot queries
//!
//! ```no_run
//! use clawde::{query, ClaudeAgentOptions, ContentBlock, Message};
//! use futures::StreamExt;
//!
//! # async fn example() -> clawde::Result<()> {
//! let mut messages = query("What is 2 + 2?", ClaudeAgentOptions::default()).await?;
//! while let Some(message) = messages.next().await {
//!     if let Message::Assistant(assistant) = message? {
//!         for block in assistant.content {
//!             if let ContentBlock::Text(text) = block {
//!                 println!("{}", text.text);
//!             }
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Interactive conversations
//!
//! Use [`ClaudeSdkClient`] for bidirectional, stateful conversations with
//! interrupts, permission callbacks, hooks, and in-process MCP servers.
//!
//! ## Requirements
//!
//! The Claude Code CLI (`claude`) must be installed and discoverable on
//! `PATH` (or supplied via [`ClaudeAgentOptions::cli_path`]).

#![warn(missing_docs)]

mod client;
mod errors;
mod internal;
mod message_parser;
mod query;
mod session_import;
mod session_mutations;
mod session_store;
mod session_summary;
mod sessions;
mod types;

pub mod mcp;
pub mod transport;

pub use client::ClaudeSdkClient;
pub use errors::{ClaudeSdkError, Result};
pub use message_parser::parse_message;
pub use query::{query, query_with_transport, MessageStream, QueryPrompt};
pub use session_import::{import_session_to_store, ImportSessionOptions};
pub use session_mutations::{
    delete_session, delete_session_via_store, fork_session, fork_session_via_store, rename_session,
    rename_session_via_store, tag_session, tag_session_via_store, ForkSessionResult,
};
pub use session_store::{file_path_to_session_key, InMemorySessionStore};
pub use session_summary::{fold_session_summary, summary_entry_to_sdk_info};
pub use sessions::{
    get_session_info, get_session_info_from_store, get_session_messages,
    get_session_messages_from_store, get_subagent_messages, get_subagent_messages_from_store,
    list_sessions, list_sessions_from_store, list_subagents, list_subagents_from_store,
    project_key_for_directory, ListSessionsOptions,
};
pub use transport::Transport;
pub use types::*;

// MCP server support at the crate root, mirroring the Python SDK's exports.
pub use mcp::{
    create_sdk_mcp_server, tool, CallToolResult, SdkMcpServer, SdkMcpTool, ToolAnnotations,
};
