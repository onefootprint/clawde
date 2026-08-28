//! [`ClaudeAgentOptions`] and its supporting configuration types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::hooks::{HookEvent, HookMatcher};
use super::mcp_config::McpServers;
use super::permissions::{CanUseTool, PermissionMode};
use super::session::{SessionStore, SessionStoreFlushMode};
use crate::errors::{ClaudeSdkError, Result};

/// SDK beta features — see <https://docs.anthropic.com/en/api/beta-headers>.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdkBeta {
    /// Enable the 1M token context window (Sonnet 4/4.5 only).
    #[serde(rename = "context-1m-2025-08-07")]
    Context1m20250807,
    /// A beta identifier this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

impl SdkBeta {
    /// The wire string for this beta.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Context1m20250807 => "context-1m-2025-08-07",
            Self::Other(s) => s,
        }
    }
}

/// Which filesystem settings source to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// Global user settings (`~/.claude/settings.json`).
    User,
    /// Project settings (`.claude/settings.json`).
    Project,
    /// Local settings (`.claude/settings.local.json`).
    Local,
}

impl SettingSource {
    /// The wire string for this source.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Controls how much effort Claude puts into its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    /// Minimal thinking, fastest responses.
    Low,
    /// Moderate thinking.
    Medium,
    /// Deep reasoning (default).
    High,
    /// Extended reasoning depth.
    Xhigh,
    /// Maximum effort.
    Max,
}

impl EffortLevel {
    /// The wire string for this level.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// System prompt configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemPrompt {
    /// A custom system prompt.
    Text(String),
    /// Claude Code's default (`claude_code` preset) system prompt.
    Preset {
        /// Instructions appended to the preset prompt.
        append: Option<String>,
        /// Strip per-user dynamic sections (working directory, auto-memory,
        /// git status) from the system prompt so it stays static and
        /// cacheable across users. The stripped content is re-injected into
        /// the first user message so the model still has access to it.
        ///
        /// Requires a Claude Code CLI version that supports this option;
        /// older CLIs silently ignore it.
        exclude_dynamic_sections: Option<bool>,
    },
    /// Load the system prompt from a file.
    File {
        /// Path to the system prompt file.
        path: String,
    },
}

impl From<String> for SystemPrompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for SystemPrompt {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

/// Base set of available built-in tools.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolsConfig {
    /// Specific tool names (e.g. `["Bash", "Read", "Edit"]`); an empty list
    /// disables all built-in tools.
    List(Vec<String>),
    /// All default Claude Code tools (the `claude_code` preset).
    Preset,
}

/// API-side task budget in tokens.
///
/// When set, the model is made aware of its remaining token budget so it can
/// pace tool use and wrap up before the limit. Sent as
/// `output_config.task_budget` with the `task-budgets-2026-03-13` beta
/// header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    /// Total token budget.
    pub total: u64,
}

/// Effort setting on an [`AgentDefinition`]: a level or a raw integer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentEffort {
    /// A named effort level.
    Level(EffortLevel),
    /// A raw integer effort value.
    Value(i64),
}

/// A reference to an MCP server on an [`AgentDefinition`]: a server name or
/// an inline `{name: config}` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMcpServerRef {
    /// A server name.
    Name(String),
    /// An inline `{name: config}` object.
    Inline(Value),
}

/// Agent definition configuration.
///
/// Serializes to the camelCase wire shape used in the `initialize` control
/// request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// When to use this agent.
    pub description: String,
    /// The agent's system prompt.
    pub prompt: String,
    /// Tools the agent may use. Deprecated: passing `"Skill"` here is
    /// deprecated; use `skills` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tools the agent may not use.
    #[serde(
        rename = "disallowedTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disallowed_tools: Option<Vec<String>>,
    /// Model alias (`"sonnet"`, `"opus"`, `"haiku"`, `"inherit"`) or a full
    /// model ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Skills enabled for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Memory scope for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<SettingSource>,
    /// MCP servers available to the agent. Each entry is a server name or an
    /// inline `{name: config}` object.
    #[serde(
        rename = "mcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<Vec<AgentMcpServerRef>>,
    /// Initial prompt for the agent.
    #[serde(
        rename = "initialPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_prompt: Option<String>,
    /// Maximum number of turns.
    #[serde(rename = "maxTurns", default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Whether the agent runs in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Effort setting for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<AgentEffort>,
    /// Permission mode for the agent.
    #[serde(
        rename = "permissionMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,
}

impl AgentDefinition {
    /// A minimal agent definition with just a description and prompt.
    pub fn new(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            prompt: prompt.into(),
            tools: None,
            disallowed_tools: None,
            model: None,
            skills: None,
            memory: None,
            mcp_servers: None,
            initial_prompt: None,
            max_turns: None,
            background: None,
            effort: None,
            permission_mode: None,
        }
    }
}

/// SDK plugin configuration. Currently only local plugins are supported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SdkPluginConfig {
    /// A plugin loaded from a local directory.
    Local {
        /// Path to the plugin directory.
        path: String,
    },
}

/// Network configuration for sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxNetworkConfig {
    /// Domain names that sandboxed processes can access.
    #[serde(
        rename = "allowedDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_domains: Option<Vec<String>>,
    /// Domains that are always blocked, even if matched by `allowed_domains`.
    #[serde(
        rename = "deniedDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub denied_domains: Option<Vec<String>>,
    /// When true in managed settings, only managed-settings allowedDomains
    /// are respected.
    #[serde(
        rename = "allowManagedDomainsOnly",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_managed_domains_only: Option<bool>,
    /// Unix socket paths accessible in sandbox (e.g. SSH agents).
    #[serde(
        rename = "allowUnixSockets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Allow all Unix sockets (less secure).
    #[serde(
        rename = "allowAllUnixSockets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_all_unix_sockets: Option<bool>,
    /// Allow binding to localhost ports (macOS only).
    #[serde(
        rename = "allowLocalBinding",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_local_binding: Option<bool>,
    /// macOS only: XPC/Mach service names to allow (supports trailing
    /// wildcard).
    #[serde(
        rename = "allowMachLookup",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_mach_lookup: Option<Vec<String>>,
    /// HTTP proxy port if bringing your own proxy.
    #[serde(
        rename = "httpProxyPort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub http_proxy_port: Option<u16>,
    /// SOCKS5 proxy port if bringing your own proxy.
    #[serde(
        rename = "socksProxyPort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub socks_proxy_port: Option<u16>,
}

/// Violations to ignore in sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxIgnoreViolations {
    /// File paths for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Vec<String>>,
    /// Network hosts for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
}

/// Sandbox settings configuration.
///
/// This controls how Claude Code sandboxes bash commands for filesystem and
/// network isolation.
///
/// **Important:** Filesystem and network restrictions are configured via
/// permission rules, not via these sandbox settings: filesystem read
/// restrictions use Read deny rules, filesystem write restrictions use Edit
/// allow/deny rules, and network restrictions use WebFetch allow/deny rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxSettings {
    /// Enable bash sandboxing (macOS/Linux only). Default: `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Auto-approve bash commands when sandboxed. Default: `true`.
    #[serde(
        rename = "autoAllowBashIfSandboxed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Commands that should run outside the sandbox (e.g. `["git",
    /// "docker"]`).
    #[serde(
        rename = "excludedCommands",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub excluded_commands: Option<Vec<String>>,
    /// Allow commands to bypass sandbox via `dangerouslyDisableSandbox`.
    /// When `false`, all commands must run sandboxed (or be in
    /// `excluded_commands`). Default: `true`.
    #[serde(
        rename = "allowUnsandboxedCommands",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_unsandboxed_commands: Option<bool>,
    /// Network configuration for sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    /// Violations to ignore.
    #[serde(
        rename = "ignoreViolations",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ignore_violations: Option<SandboxIgnoreViolations>,
    /// Enable weaker sandbox for unprivileged Docker environments (Linux
    /// only). Reduces security. Default: `false`.
    #[serde(
        rename = "enableWeakerNestedSandbox",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_weaker_nested_sandbox: Option<bool>,
}

/// Controls whether thinking text is returned summarized or omitted. Opus
/// 4.7+ defaults to omitted (signature-only); pass `Summarized` to receive
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// Return a readable summary of the reasoning.
    Summarized,
    /// Return thinking blocks with empty text.
    Omitted,
}

impl ThinkingDisplay {
    /// The wire string for this display mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// Controls Claude's thinking/reasoning behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingConfig {
    /// Claude decides when and how much to think (Opus 4.6+). Default for
    /// models that support it.
    Adaptive {
        /// Thinking display mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// Fixed thinking token budget (older models).
    Enabled {
        /// The thinking token budget.
        budget_tokens: u64,
        /// Thinking display mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// No extended thinking.
    Disabled,
}

/// The [`ClaudeAgentOptions::skills`] configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillsConfig {
    /// Enable every discovered skill.
    All,
    /// Enable only the listed skills. Names match the SKILL.md `name` /
    /// directory name, or `plugin:skill` for plugin-qualified skills. Names
    /// must be exact: wildcards, delimiters, and surrounding whitespace fail
    /// at connect.
    List(Vec<String>),
}

/// Callback for stderr output from the Claude Code subprocess. Receives one
/// line at a time.
pub type StderrCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Query options for the Claude Agent SDK.
///
/// Construct with struct-update syntax over [`Default`]:
///
/// ```no_run
/// use clawde::ClaudeAgentOptions;
///
/// let options = ClaudeAgentOptions {
///     model: Some("claude-sonnet-4-5".into()),
///     max_turns: Some(10),
///     ..Default::default()
/// };
/// ```
#[derive(Clone, Default)]
pub struct ClaudeAgentOptions {
    /// Specify the base set of available built-in tools. To restrict which
    /// tools the model may call without being prompted, use
    /// [`ClaudeAgentOptions::allowed_tools`] instead.
    pub tools: Option<ToolsConfig>,

    /// Tool names that are auto-allowed without prompting for permission.
    ///
    /// These tools execute automatically without asking the user for
    /// approval. To restrict which tools are available at all, use
    /// [`ClaudeAgentOptions::tools`].
    ///
    /// Passing `"Skill"` here is deprecated. Use
    /// [`ClaudeAgentOptions::skills`] instead, which configures everything
    /// needed (including allowing the `Skill` tool).
    pub allowed_tools: Vec<String>,

    /// System prompt configuration.
    pub system_prompt: Option<SystemPrompt>,

    /// MCP (Model Context Protocol) server configurations. Keys are server
    /// names, values are server configurations. May also be a path to an MCP
    /// config JSON file.
    pub mcp_servers: McpServers,

    /// When `true`, only use MCP servers passed via
    /// [`ClaudeAgentOptions::mcp_servers`], ignoring all other MCP
    /// configurations the CLI would otherwise load (e.g. project
    /// `.mcp.json`, user/global settings, plugin-provided servers). Maps to
    /// the CLI's `--strict-mcp-config` flag.
    pub strict_mcp_config: bool,

    /// Permission mode for the session.
    pub permission_mode: Option<PermissionMode>,

    /// Continue the most recent conversation in the current directory
    /// instead of starting a new one. Mutually exclusive with
    /// [`ClaudeAgentOptions::resume`].
    pub continue_conversation: bool,

    /// Session ID to resume. Loads the conversation history from the
    /// specified session.
    pub resume: Option<String>,

    /// Use a specific session ID for the conversation instead of an
    /// auto-generated one. Must be a valid UUID. Cannot be used with
    /// `continue_conversation` or `resume` unless `fork_session` is also
    /// set.
    pub session_id: Option<String>,

    /// Maximum number of conversation turns before the query stops. A turn
    /// consists of a user message and assistant response.
    pub max_turns: Option<u32>,

    /// Maximum budget in USD for the query. The query will stop if this
    /// budget is exceeded, returning an `error_max_budget_usd` result.
    pub max_budget_usd: Option<f64>,

    /// Tool names that are disallowed. These tools are removed from the
    /// model's context and cannot be used, even if they would otherwise be
    /// allowed.
    pub disallowed_tools: Vec<String>,

    /// Claude model to use. Defaults to the CLI default model.
    pub model: Option<String>,

    /// Fallback model to use if the primary model fails or is unavailable.
    pub fallback_model: Option<String>,

    /// Enable beta features. See
    /// <https://docs.anthropic.com/en/api/beta-headers>.
    pub betas: Vec<SdkBeta>,

    /// MCP tool name to use for permission prompts. When set, permission
    /// requests are routed through this MCP tool instead of the default
    /// handler.
    pub permission_prompt_tool_name: Option<String>,

    /// Current working directory for the session. Defaults to the process
    /// cwd.
    pub cwd: Option<PathBuf>,

    /// Path to the Claude Code CLI executable. Discovered on `PATH` and in
    /// well-known locations if not specified.
    pub cli_path: Option<PathBuf>,

    /// Path to an additional settings JSON file to load (or an inline JSON
    /// string). These are loaded into the "flag settings" layer, which has
    /// the highest priority among user-controlled settings. Equivalent to
    /// the `--settings` CLI flag.
    pub settings: Option<String>,

    /// Additional directories Claude can access beyond the current working
    /// directory. Paths should be absolute.
    pub add_dirs: Vec<PathBuf>,

    /// Environment variables to pass to the Claude Code subprocess.
    ///
    /// SDK consumers can identify their app/library in the User-Agent header
    /// by setting `CLAUDE_AGENT_SDK_CLIENT_APP` (e.g. `"my-app/1.0.0"`).
    pub env: HashMap<String, String>,

    /// Additional CLI arguments to pass to Claude Code. Keys are argument
    /// names (without `--`), values are argument values. Use `None` for
    /// boolean flags.
    pub extra_args: HashMap<String, Option<String>>,

    /// Maximum bytes to buffer when reading the CLI subprocess stdout.
    pub max_buffer_size: Option<usize>,

    /// Callback for stderr output from the Claude Code subprocess. Useful
    /// for debugging and logging.
    pub stderr: Option<StderrCallback>,

    /// Custom permission handler for tool calls that would otherwise prompt
    /// the user.
    ///
    /// Invoked when the CLI's permission rules evaluate to "ask" for a tool
    /// call — it is the SDK replacement for the interactive permission
    /// prompt. It is *not* invoked for tool calls already permitted by
    /// `allowed_tools`, `permission_mode` (e.g. `AcceptEdits` /
    /// `BypassPermissions`), or `permissions.allow` rules in settings, since
    /// those never reach a prompt. A warning is logged when the client
    /// connects if this callback is set alongside options that visibly
    /// shadow it. To observe or gate *every* tool call regardless of
    /// permission rules, use a `PreToolUse` hook via
    /// [`ClaudeAgentOptions::hooks`] instead — but note that a `PreToolUse`
    /// hook returning an *allow* decision also skips this callback.
    pub can_use_tool: Option<CanUseTool>,

    /// Hook callbacks for responding to various events during execution.
    /// Hooks can modify behavior, add context, or implement custom logic.
    /// See <https://docs.anthropic.com/en/docs/claude-code/hooks>.
    ///
    /// **Dispatch order:** multiple matchers registered on the same event
    /// are dispatched **concurrently** by the CLI — all `hook_callback`
    /// control requests for a given event fire in parallel, not
    /// sequentially. Design each hook to be independent; do not rely on one
    /// completing before another starts.
    pub hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,

    /// Optional user identifier associated with the session. On Unix this is
    /// passed as the uid to run the subprocess as.
    pub user: Option<String>,

    /// Include partial/streaming message events in the output. When `true`,
    /// [`crate::StreamEvent`]s are emitted during streaming.
    pub include_partial_messages: bool,

    /// Include hook lifecycle events in the message stream. When `true`, the
    /// CLI emits hook events (PreToolUse, PostToolUse, Stop, etc.) as
    /// [`crate::HookEventMessage`]s in the message stream.
    pub include_hook_events: bool,

    /// Forward subagent text and thinking blocks as messages in the stream.
    ///
    /// By default only `tool_use` / `tool_result` blocks from subagents
    /// (spawned via the Agent tool) are emitted, as
    /// [`crate::AssistantMessage`] / [`crate::UserMessage`] objects whose
    /// `parent_tool_use_id` is the spawning Agent `tool_use` id — enough for
    /// a progress heartbeat. When `true`, the subagent's text and thinking
    /// blocks are forwarded the same way, so consumers can render the full
    /// nested transcript.
    pub forward_subagent_text: bool,

    /// When `true`, resumed sessions fork to a new session ID rather than
    /// continuing the previous session. Use with
    /// [`ClaudeAgentOptions::resume`].
    pub fork_session: bool,

    /// When resuming, only load the conversation up to and including the
    /// message with this UUID. Use with `resume` (and usually
    /// `fork_session`) to branch from an earlier point in the conversation.
    pub resume_session_at: Option<String>,

    /// With `resume_session_at`: the UUID of the user prompt whose turn this
    /// truncating resume intends to discard.
    ///
    /// When set, the CLI validates at load time that every transcript entry
    /// after the `resume_session_at` point is attributable to that turn, and
    /// refuses the resume otherwise. A refusal surfaces as an error whose
    /// message contains `Resume rejected by --resume-drops-turn:` — match on
    /// that text. Treat it as deterministic: clear the pending fork target
    /// and resume plainly rather than retrying the same request. Leave unset
    /// to keep the unvalidated truncation behavior.
    pub resume_drops_turn: Option<String>,

    /// Programmatically define custom subagents invokable via the Agent
    /// tool. Keys are agent names, values are agent definitions.
    pub agents: Option<HashMap<String, AgentDefinition>>,

    /// Control which filesystem settings to load. When `None`, all sources
    /// are loaded (matches CLI defaults). Pass an empty vec to disable
    /// filesystem settings (SDK isolation mode). Must include
    /// [`SettingSource::Project`] to load CLAUDE.md files.
    pub setting_sources: Option<Vec<SettingSource>>,

    /// Skills to enable for the main session.
    ///
    /// This is the single place to turn skills on; you do not need to add
    /// `"Skill"` to `allowed_tools` or set `setting_sources` yourself — the
    /// SDK does both when this is set.
    ///
    /// - `None` (default): no SDK auto-configuration. The CLI's own defaults
    ///   still apply, so this is **not** "skills off" — to suppress every
    ///   skill from the listing, use an empty list.
    /// - [`SkillsConfig::All`]: enable every discovered skill.
    /// - [`SkillsConfig::List`]: enable only the listed skills.
    ///
    /// This is a **context filter**, not a sandbox: unlisted skills are
    /// hidden from the model's listing and rejected by the Skill tool, but
    /// their files remain on disk and are reachable via Read/Bash. Do not
    /// store secrets in skill files.
    pub skills: Option<SkillsConfig>,

    /// Sandbox settings for command execution isolation.
    pub sandbox: Option<SandboxSettings>,

    /// Load plugins for this session. Plugins provide custom commands,
    /// agents, skills, and hooks that extend Claude Code's capabilities.
    /// Currently only local plugins are supported.
    pub plugins: Vec<SdkPluginConfig>,

    /// Maximum tokens the model may use for its thinking/reasoning process.
    ///
    /// Deprecated: use [`ClaudeAgentOptions::thinking`] instead. On newer
    /// models, this is treated as on/off (0 = disabled, any other value =
    /// adaptive).
    pub max_thinking_tokens: Option<u64>,

    /// Controls Claude's thinking/reasoning behavior. When set, takes
    /// precedence over the deprecated `max_thinking_tokens`. See
    /// <https://docs.anthropic.com/en/docs/build-with-claude/adaptive-thinking>.
    pub thinking: Option<ThinkingConfig>,

    /// Controls how much effort Claude puts into its response. Works with
    /// adaptive thinking to guide thinking depth.
    pub effort: Option<EffortLevel>,

    /// Output format configuration for structured responses. When specified,
    /// the agent returns structured data matching the schema. Matches the
    /// Messages API structure, e.g.
    /// `{"type": "json_schema", "schema": {"type": "object", ...}}`.
    pub output_format: Option<Value>,

    /// Enable file checkpointing to track file changes during the session.
    ///
    /// When enabled, files can be rewound to their state at any user message
    /// using [`crate::ClaudeSdkClient::rewind_files`]. File checkpointing
    /// creates backups of files before they are modified so they can be
    /// restored later.
    pub enable_file_checkpointing: bool,

    /// Mirror session transcripts to an external store.
    ///
    /// When set, every transcript line written locally is also passed to
    /// [`SessionStore::append`], and `resume` can materialize from the store
    /// when the local file is absent.
    pub session_store: Option<Arc<dyn SessionStore>>,

    /// When to flush mirrored transcript entries to `session_store`. Ignored
    /// when `session_store` is `None`.
    pub session_store_flush: SessionStoreFlushMode,

    /// Timeout for each [`SessionStore::load`] / [`SessionStore::list_subkeys`]
    /// call during resume materialization, in milliseconds. `None` uses the
    /// default of 60 000 ms. If the adapter doesn't settle within this
    /// window the query fails with a clear error instead of hanging forever.
    pub load_timeout_ms: Option<u64>,

    /// API-side task budget in tokens.
    pub task_budget: Option<TaskBudget>,
}

impl std::fmt::Debug for ClaudeAgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgentOptions")
            .field("tools", &self.tools)
            .field("allowed_tools", &self.allowed_tools)
            .field("system_prompt", &self.system_prompt)
            .field("permission_mode", &self.permission_mode)
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .field("resume", &self.resume)
            .field("continue_conversation", &self.continue_conversation)
            .finish_non_exhaustive()
    }
}

impl ClaudeAgentOptions {
    /// The effective `load_timeout_ms` (default 60 000).
    pub(crate) fn effective_load_timeout_ms(&self) -> u64 {
        self.load_timeout_ms.unwrap_or(60_000)
    }
}

/// Return the tool an `allowed_tools` entry allows outright, else `None`.
///
/// Mirrors the CLI's rule parser: an entry allows a whole tool when it has no
/// `(...)` specifier (`"Read"`), or when the specifier is empty or a lone
/// wildcard (`"Read()"`, `"Read(*)"`). A real specifier (`"Bash(ls:*)"`) only
/// allows matching invocations. Malformed entries fall back to the whole
/// string as a tool name in the CLI, so they match nothing and are ignored.
fn whole_tool_allowed(entry: &str) -> Option<&str> {
    if entry.trim().is_empty() {
        return None;
    }
    let Some(open_index) = entry.find('(') else {
        return Some(entry);
    };
    if open_index == 0 || !entry.ends_with(')') {
        return None;
    }
    let specifier = &entry[open_index + 1..entry.len() - 1];
    if specifier.is_empty() || specifier == "*" {
        Some(&entry[..open_index])
    } else {
        None
    }
}

/// Return the shadowing warning message for these options, or `None`.
fn get_can_use_tool_shadowed_warning(
    permission_mode: Option<PermissionMode>,
    allowed_tools: &[String],
) -> Option<String> {
    if permission_mode == Some(PermissionMode::BypassPermissions) {
        return Some(
            "can_use_tool will not be invoked: permission_mode 'bypassPermissions' \
             auto-approves every tool call (except explicit deny rules) before the \
             callback is consulted. To gate every tool call, use a PreToolUse hook \
             instead."
                .to_string(),
        );
    }
    // Dedupe while preserving order: redundant configs like ["Read", "Read()"]
    // resolve to the same tool and must not report it twice.
    let mut seen = std::collections::HashSet::new();
    let shadowed: Vec<&str> = allowed_tools
        .iter()
        .filter_map(|entry| whole_tool_allowed(entry))
        .filter(|tool| seen.insert(tool.to_string()))
        .collect();
    if shadowed.is_empty() {
        return None;
    }
    Some(format!(
        "can_use_tool will not be invoked for: {}. An allowed_tools entry that \
         allows a whole tool auto-approves it before the callback is consulted. To \
         gate every tool call, use a PreToolUse hook; or narrow the entry so calls \
         fall through to can_use_tool. Allow rules from settings files can also \
         shadow the callback but are not visible here.",
        shadowed.join(", ")
    ))
}

/// Warn (via `tracing`) if `can_use_tool` is shadowed. Called once per query
/// construction. Advisory only: shadowing can be intentional, e.g. a callback
/// used solely for tools outside `allowed_tools`.
fn warn_if_can_use_tool_shadowed(options: &ClaudeAgentOptions) {
    if options.can_use_tool.is_none() {
        return;
    }
    // SkillsConfig::All makes the transport append a bare "Skill" to the
    // effective allowed_tools, so it shadows the callback just like a
    // hand-written entry. A skill list appends Skill(name) specifiers, which
    // do not.
    let mut allowed_tools = options.allowed_tools.clone();
    if matches!(options.skills, Some(SkillsConfig::All))
        && !allowed_tools.iter().any(|t| t == "Skill")
    {
        allowed_tools.push("Skill".to_string());
    }
    if let Some(message) =
        get_can_use_tool_shadowed_warning(options.permission_mode, &allowed_tools)
    {
        tracing::warn!(target: "clawde", "{message}");
    }
}

/// Validate `can_use_tool` and route permission prompts over stdio.
///
/// Shared by [`crate::query`] and [`crate::ClaudeSdkClient::connect`] so both
/// entry points enforce the same rules. Returns `options` unchanged when no
/// callback is set; otherwise checks it is not combined with
/// `permission_prompt_tool_name`, emits the shadowing advisory, and returns a
/// copy with `permission_prompt_tool_name = "stdio"` so the CLI sends
/// permission requests over the control protocol.
pub(crate) fn configure_can_use_tool(options: ClaudeAgentOptions) -> Result<ClaudeAgentOptions> {
    if options.can_use_tool.is_none() {
        return Ok(options);
    }
    if options.permission_prompt_tool_name.is_some() {
        return Err(ClaudeSdkError::InvalidConfig(
            "can_use_tool callback cannot be used with permission_prompt_tool_name. \
             Please use one or the other."
                .to_string(),
        ));
    }
    warn_if_can_use_tool_shadowed(&options);
    Ok(ClaudeAgentOptions {
        permission_prompt_tool_name: Some("stdio".to_string()),
        ..options
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_tool_allowed_matches_cli_rule_parser() {
        assert_eq!(whole_tool_allowed("Read"), Some("Read"));
        assert_eq!(whole_tool_allowed("Read()"), Some("Read"));
        assert_eq!(whole_tool_allowed("Read(*)"), Some("Read"));
        assert_eq!(whole_tool_allowed("Bash(ls:*)"), None);
        assert_eq!(whole_tool_allowed(""), None);
        assert_eq!(whole_tool_allowed("(oops)"), None);
        assert_eq!(whole_tool_allowed("Read(x"), None);
    }
}
