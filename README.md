# Claude Agent SDK for Rust

A Rust port of the [Claude Agent SDK for Python](https://github.com/anthropics/claude-agent-sdk-python): programmatic access to the Claude Code CLI with a 1:1 public interface, expressed idiomatically in Rust on top of tokio.

## Installation

```toml
[dependencies]
claude_agent_sdk = { path = "." }
futures = "0.3"
tokio = { version = "1", features = ["full"] }
```

**Prerequisites:** Rust 1.85+, and Claude Code 2.0.0+ (`npm install -g @anthropic-ai/claude-code`, or set `ClaudeAgentOptions.cli_path`).

## Quick start

```rust
use claude_agent_sdk::{query, ClaudeAgentOptions, ContentBlock, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut messages = query("What is 2 + 2?", ClaudeAgentOptions::default()).await?;
    while let Some(message) = messages.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("{}", text.text);
                }
            }
        }
    }
    Ok(())
}
```

See `examples/` for interactive clients (`streaming_mode`), in-process MCP servers (`mcp_calculator`), permission callbacks (`tool_permission_callback`), and hooks (`hooks`).

## API mapping (Python → Rust)

| Python | Rust |
|---|---|
| `query(prompt=..., options=...)` | `query(prompt, options)` → `MessageStream` (a `Stream<Item = Result<Message>>`) |
| `query(..., transport=...)` | `query_with_transport(prompt, options, Some(transport))` |
| `ClaudeSDKClient` | `ClaudeSdkClient` (`connect` / `query` / `receive_messages` / `receive_response` / `interrupt` / `set_permission_mode` / `set_model` / `rewind_files` / `reconnect_mcp_server` / `toggle_mcp_server` / `stop_task` / `get_mcp_status` / `get_context_usage` / `get_server_info` / `disconnect`) |
| `ClaudeAgentOptions(...)` | `ClaudeAgentOptions { .., ..Default::default() }` |
| `@tool(...)` decorator | `mcp::tool(name, description, schema, handler)` |
| `create_sdk_mcp_server(...)` | `mcp::create_sdk_mcp_server(name, version, tools)` |
| bring-your-own `mcp.server.Server` | implement the `SdkMcpServer` trait (JSON-RPC in/out) |
| `Message` union / `SystemMessage` subclasses | `Message` enum (task/mirror/hook messages are sibling variants) |
| `PermissionUpdate` dataclass + `to_dict`/`from_dict` | `PermissionUpdate` enum with serde matching the wire format |
| Hook TypedDicts (`async_`, `continue_`) | typed structs/enums; serde renames to the CLI names (`async`, `continue`) |
| `can_use_tool` / `HookCallback` callables | `CanUseTool` / `HookCallback` (`Arc<dyn Fn(..) -> BoxFuture<..>>`) |
| exceptions (`CLINotFoundError`, `ProcessError`, `ResultError`, ...) | `ClaudeSdkError` enum variants (`CliNotFound`, `Process`, `ResultError`, ...) with accessor methods (`subtype()`, `terminal_reason()`, ...) |
| `Transport` ABC | `Transport` trait (`async_trait`) |
| `SessionStore` Protocol (optional methods probed at runtime) | `SessionStore` trait; optional methods default to `StoreUnimplemented` and are gated by `implements(SessionStoreMethod)` |
| `list_sessions` / `get_session_info` / `get_session_messages` / `list_subagents` / `get_subagent_messages` (+ `_from_store` variants) | same names; filesystem variants are sync, store variants async |
| `rename_session` / `tag_session` / `delete_session` / `fork_session` (+ `_via_store`) | same names |
| `import_session_to_store`, `fold_session_summary`, `project_key_for_directory`, `InMemorySessionStore` | same names |
| `CanUseToolShadowedWarning` (Python warning) | `tracing::warn!` on connect |

## Intentional deviations

- **Async runtime:** tokio replaces anyio; Python's `_task_compat`/`_mcp_compat` runtime shims have no counterpart.
- **SDK MCP servers** are served by a native JSON-RPC handler (initialize / ping / tools/list / tools/call) instead of bridging into the `mcp` package. Bring-your-own servers implement `SdkMcpServer` (with an optional `close()` lifecycle hook called at shutdown).
- **Tool input schemas** are JSON Schema values (`serde_json::json!`); the Python-type shorthand maps to `{"param": <schema fragment>}` maps.
- `CLAUDE_CODE_ENTRYPOINT` is `sdk-rust`.
- `ClaudeAgentOptions.user` accepts a username or numeric uid on Unix (usernames resolve via `getpwnam_r`); `debug_stderr` (deprecated in Python) is dropped — use the `stderr` callback.
- OpenTelemetry trace-context propagation into the subprocess is not implemented.
- Cleanup of a dropped `MessageStream`/`ClaudeSdkClient` is spawned on the runtime handle captured at construction (works from non-runtime threads too); prefer draining the stream or calling `disconnect()`. The child process is `kill_on_drop` as a backstop (in place of Python's `atexit` reaper).
- The `testing.session_store_conformance` helper suite is not ported.

## Development

```sh
cargo clippy --all-targets   # lint (zero warnings)
cargo test                   # unit + integration tests (fake-CLI e2e on Unix)
cargo +nightly fmt --all     # format
```
