# Clawde

[![CI](https://github.com/onefootprint/clawde/actions/workflows/ci.yml/badge.svg)](https://github.com/onefootprint/clawde/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/clawde.svg)](https://crates.io/crates/clawde)
[![docs.rs](https://img.shields.io/docsrs/clawde)](https://docs.rs/clawde)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A Rust port of the [Claude Agent SDK for Python](https://github.com/anthropics/claude-agent-sdk-python): programmatic access to the Claude Code CLI with a 1:1 public interface, expressed idiomatically in Rust on top of tokio.

> **Note:** Clawde is an unofficial community port and is not affiliated with or
> endorsed by Anthropic. For the official SDKs, see the
> [Claude Agent SDK docs](https://code.claude.com/docs/en/agent-sdk).

## Installation

```toml
[dependencies]
clawde = "0.1"
futures = "0.3"
tokio = { version = "1", features = ["full"] }
```

**Prerequisites:** Rust 1.85+, and Claude Code 2.0.0+ (`npm install -g @anthropic-ai/claude-code`, or set `ClaudeAgentOptions.cli_path`).

## Quick start

```rust
use clawde::{query, ClaudeAgentOptions, ContentBlock, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> clawde::Result<()> {
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

## Features

- **One-shot queries** — `query()` returns a `Stream` of typed `Message`s.
- **Interactive client** — `ClaudeSdkClient` for multi-turn conversations:
  interrupts, permission-mode and model switching, file rewind, MCP server
  management, context usage.
- **In-process MCP servers** — define tools as Rust closures with
  `mcp::tool` / `mcp::create_sdk_mcp_server`, or implement the `SdkMcpServer`
  trait for full control.
- **Hooks and permission callbacks** — intercept and gate tool use with
  `HookCallback` and `can_use_tool`.
- **Sessions** — list, inspect, fork, rename, tag, delete, and resume
  sessions, from the filesystem or a custom `SessionStore`.

The API mirrors the Python SDK one-to-one — same names, same options, same
wire behavior — so the [official docs](https://code.claude.com/docs/en/agent-sdk)
apply directly; see [docs.rs/clawde](https://docs.rs/clawde) for the Rust
signatures.

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for ground rules. A weekly
[upstream-watch workflow](.github/workflows/upstream-watch.yml) tracks new
releases of the Python SDK and the Claude Code CLI: it tests against the new
CLI, runs an AI-assisted parity audit of the upstream diff, and files an issue
plus a version-bump PR when the port needs to catch up.

## Versioning

The crate tracks the Python SDK's API surface; `.github/upstream/state.json`
records the upstream tag and CLI version the current code was last audited
against. Until 1.0, minor versions may contain breaking changes.

## License

MIT — see [LICENSE](LICENSE). Portions are derived from
[claude-agent-sdk-python](https://github.com/anthropics/claude-agent-sdk-python),
© Anthropic, PBC, also MIT-licensed.
