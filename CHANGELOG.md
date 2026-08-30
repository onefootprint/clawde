# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-30

### Added

- Initial release: a 1:1 Rust port of the
  [Claude Agent SDK for Python](https://github.com/anthropics/claude-agent-sdk-python)
  (audited against `v0.2.148`, Claude Code CLI 2.1.251).
- `query()` / `query_with_transport()` one-shot streaming API.
- `ClaudeSdkClient` bidirectional client (interrupt, permission mode, model
  switching, file rewind, MCP server management, context usage).
- In-process SDK MCP servers (`mcp::tool`, `mcp::create_sdk_mcp_server`, and the
  `SdkMcpServer` trait) served over a native JSON-RPC handler.
- Hooks, `can_use_tool` permission callbacks, and the full control protocol.
- Session management: listing, messages, fork/rename/tag/delete, session
  stores (`SessionStore` trait, `InMemorySessionStore`), transcript mirroring,
  and resume materialization.
