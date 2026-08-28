//! Hook event, input, and output types.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::errors::Result;

/// Hook events supported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Before a tool call executes.
    PreToolUse,
    /// After a tool call succeeds.
    PostToolUse,
    /// After a tool call fails.
    PostToolUseFailure,
    /// When the user submits a prompt.
    UserPromptSubmit,
    /// When the main agent stops.
    Stop,
    /// When a subagent stops.
    SubagentStop,
    /// Before conversation compaction.
    PreCompact,
    /// On CLI notifications.
    Notification,
    /// When a subagent starts.
    SubagentStart,
    /// When a permission prompt would be shown.
    PermissionRequest,
}

impl HookEvent {
    /// The wire name of this event.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::Notification => "Notification",
            Self::SubagentStart => "SubagentStart",
            Self::PermissionRequest => "PermissionRequest",
        }
    }
}

/// Base hook input fields present across many hook events.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BaseHookInput {
    /// Session the hook fired in.
    #[serde(default)]
    pub session_id: String,
    /// Path to the session transcript.
    #[serde(default)]
    pub transcript_path: String,
    /// Working directory of the session.
    #[serde(default)]
    pub cwd: String,
    /// Current permission mode, when reported.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Sub-agent identifier. Present only when the hook fires from inside a
    /// Task-spawned sub-agent; absent on the main thread. Matches the
    /// `agent_id` emitted by that sub-agent's SubagentStart/SubagentStop
    /// hooks. When multiple sub-agents run in parallel their tool-lifecycle
    /// hooks interleave over the same control channel — this is the only
    /// reliable way to attribute each one to the correct sub-agent.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Agent type name (e.g. "general-purpose", "code-reviewer"). Present
    /// inside a sub-agent (alongside `agent_id`), or on the main thread of a
    /// session started with `--agent` (without `agent_id`).
    #[serde(default)]
    pub agent_type: Option<String>,
}

/// Input data for PreToolUse hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct PreToolUseHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Name of the tool being called.
    pub tool_name: String,
    /// Tool input parameters.
    #[serde(default)]
    pub tool_input: Value,
    /// Identifier of this tool call.
    #[serde(default)]
    pub tool_use_id: String,
}

/// Input data for PostToolUse hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct PostToolUseHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Name of the tool that was called.
    pub tool_name: String,
    /// Tool input parameters.
    #[serde(default)]
    pub tool_input: Value,
    /// The tool's response.
    #[serde(default)]
    pub tool_response: Value,
    /// Identifier of this tool call.
    #[serde(default)]
    pub tool_use_id: String,
}

/// Input data for PostToolUseFailure hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct PostToolUseFailureHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Name of the tool that failed.
    pub tool_name: String,
    /// Tool input parameters.
    #[serde(default)]
    pub tool_input: Value,
    /// Identifier of this tool call.
    #[serde(default)]
    pub tool_use_id: String,
    /// The failure description.
    #[serde(default)]
    pub error: String,
    /// Whether the failure came from an interrupt.
    #[serde(default)]
    pub is_interrupt: Option<bool>,
}

/// Input data for UserPromptSubmit hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct UserPromptSubmitHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// The submitted prompt.
    #[serde(default)]
    pub prompt: String,
}

/// Input data for Stop hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct StopHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Whether a stop hook is already active.
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// Input data for SubagentStop hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStopHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Whether a stop hook is already active.
    #[serde(default)]
    pub stop_hook_active: bool,
    /// The stopping subagent's transcript path.
    #[serde(default)]
    pub agent_transcript_path: String,
}

/// Input data for PreCompact hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct PreCompactHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// What triggered compaction (`"manual"` or `"auto"`).
    #[serde(default)]
    pub trigger: String,
    /// Custom compaction instructions, if any.
    #[serde(default)]
    pub custom_instructions: Option<String>,
}

/// Input data for Notification hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// The notification message.
    #[serde(default)]
    pub message: String,
    /// Optional notification title.
    #[serde(default)]
    pub title: Option<String>,
    /// The notification type.
    #[serde(default)]
    pub notification_type: String,
}

/// Input data for SubagentStart hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStartHookInput {
    /// Fields shared across hook events (carries the required `agent_id` /
    /// `agent_type`).
    #[serde(flatten)]
    pub base: BaseHookInput,
}

/// Input data for PermissionRequest hook events.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequestHookInput {
    /// Fields shared across hook events.
    #[serde(flatten)]
    pub base: BaseHookInput,
    /// Name of the tool requesting permission.
    pub tool_name: String,
    /// Tool input parameters.
    #[serde(default)]
    pub tool_input: Value,
    /// Permission suggestions from the CLI.
    #[serde(default)]
    pub permission_suggestions: Option<Vec<Value>>,
}

/// Strongly-typed hook input, discriminated by `hook_event_name`.
///
/// Inputs that fail to parse into a known typed variant (e.g. events added by
/// a newer CLI) are delivered as [`HookInput::Unknown`] so hooks always see
/// the raw payload.
#[derive(Debug, Clone)]
pub enum HookInput {
    /// PreToolUse input.
    PreToolUse(PreToolUseHookInput),
    /// PostToolUse input.
    PostToolUse(PostToolUseHookInput),
    /// PostToolUseFailure input.
    PostToolUseFailure(PostToolUseFailureHookInput),
    /// UserPromptSubmit input.
    UserPromptSubmit(UserPromptSubmitHookInput),
    /// Stop input.
    Stop(StopHookInput),
    /// SubagentStop input.
    SubagentStop(SubagentStopHookInput),
    /// PreCompact input.
    PreCompact(PreCompactHookInput),
    /// Notification input.
    Notification(NotificationHookInput),
    /// SubagentStart input.
    SubagentStart(SubagentStartHookInput),
    /// PermissionRequest input.
    PermissionRequest(PermissionRequestHookInput),
    /// Unrecognized or unparseable hook input; carries the raw payload.
    Unknown(Value),
}

impl HookInput {
    /// Parse a raw hook input payload, falling back to
    /// [`HookInput::Unknown`] when the payload doesn't match a known shape.
    pub fn from_value(value: Value) -> Self {
        fn parse<T: serde::de::DeserializeOwned>(
            value: &Value,
            wrap: impl Fn(T) -> HookInput,
        ) -> Option<HookInput> {
            serde_json::from_value(value.clone()).ok().map(wrap)
        }
        let event = value.get("hook_event_name").and_then(Value::as_str);
        let parsed = match event {
            Some("PreToolUse") => parse(&value, HookInput::PreToolUse),
            Some("PostToolUse") => parse(&value, HookInput::PostToolUse),
            Some("PostToolUseFailure") => parse(&value, HookInput::PostToolUseFailure),
            Some("UserPromptSubmit") => parse(&value, HookInput::UserPromptSubmit),
            Some("Stop") => parse(&value, HookInput::Stop),
            Some("SubagentStop") => parse(&value, HookInput::SubagentStop),
            Some("PreCompact") => parse(&value, HookInput::PreCompact),
            Some("Notification") => parse(&value, HookInput::Notification),
            Some("SubagentStart") => parse(&value, HookInput::SubagentStart),
            Some("PermissionRequest") => parse(&value, HookInput::PermissionRequest),
            _ => None,
        };
        parsed.unwrap_or(HookInput::Unknown(value))
    }
}

/// Decision value for PreToolUse hook-specific output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    /// Allow the tool call.
    Allow,
    /// Deny the tool call.
    Deny,
    /// Prompt for the tool call.
    Ask,
    /// Defer the tool call: the run stops and the result carries the deferred
    /// call (see [`crate::DeferredToolUse`]).
    Defer,
}

/// Event-specific hook output, discriminated by `hookEventName` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    /// Hook-specific output for PreToolUse events.
    PreToolUse {
        /// Permission decision for the tool call.
        #[serde(rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
        permission_decision: Option<PermissionDecision>,
        /// Reason for the decision.
        #[serde(
            rename = "permissionDecisionReason",
            skip_serializing_if = "Option::is_none"
        )]
        permission_decision_reason: Option<String>,
        /// Replacement tool input.
        #[serde(rename = "updatedInput", skip_serializing_if = "Option::is_none")]
        updated_input: Option<Map<String, Value>>,
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for PostToolUse events.
    PostToolUse {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
        /// Replaces the tool output before it is sent to the model.
        ///
        /// For built-in tools (Bash, Read, Edit, etc.) the value must match
        /// the tool's output schema (e.g. `{"stdout": ..., "stderr": ...,
        /// "interrupted": ...}` for Bash); a mismatched shape is rejected and
        /// the original output is kept.
        #[serde(rename = "updatedToolOutput", skip_serializing_if = "Option::is_none")]
        updated_tool_output: Option<Value>,
        /// Replaces the output for MCP tools only. Prefer
        /// `updated_tool_output`, which works for all tools.
        #[serde(
            rename = "updatedMCPToolOutput",
            skip_serializing_if = "Option::is_none"
        )]
        updated_mcp_tool_output: Option<Value>,
    },
    /// Hook-specific output for PostToolUseFailure events.
    PostToolUseFailure {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for UserPromptSubmit events.
    UserPromptSubmit {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for SessionStart events.
    SessionStart {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for Notification events.
    Notification {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for SubagentStart events.
    SubagentStart {
        /// Extra context injected into the conversation.
        #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
    },
    /// Hook-specific output for PermissionRequest events.
    PermissionRequest {
        /// The permission decision object.
        decision: Value,
    },
}

/// Synchronous hook output with control and decision fields.
///
/// Field names serialize to the CLI's expected wire names (`continue`,
/// `suppressOutput`, ...).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncHookJsonOutput {
    /// Whether Claude should proceed after hook execution (default `true`).
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
    /// Hide stdout from transcript mode (default `false`).
    #[serde(rename = "suppressOutput", skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
    /// Message shown when `continue_` is `false`.
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Set to `"block"` to indicate blocking behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Warning message displayed to the user.
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Feedback message for Claude about the decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Event-specific controls (e.g. `permissionDecision` for PreToolUse).
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Async hook output that defers hook execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncHookJsonOutput {
    /// Always `true`; marks the hook as deferred.
    #[serde(rename = "async")]
    pub async_: bool,
    /// Optional timeout in milliseconds for the async operation.
    #[serde(rename = "asyncTimeout", skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<u64>,
}

/// Output of a hook callback.
// Not boxed: hook outputs are built by hand in user callbacks, and
// `Sync(Box::new(..))` would tax every hook author for a value created once
// per hook invocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookJsonOutput {
    /// Defer hook execution.
    Async(AsyncHookJsonOutput),
    /// Synchronous output with control and decision fields.
    Sync(SyncHookJsonOutput),
}

impl Default for HookJsonOutput {
    fn default() -> Self {
        Self::Sync(SyncHookJsonOutput::default())
    }
}

/// Context information for hook callbacks. Currently a placeholder; reserved
/// for future abort-signal support.
#[derive(Debug, Clone, Default)]
pub struct HookContext {}

/// A hook callback. Receives the typed hook input, the optional tool use id,
/// and the hook context.
pub type HookCallback = Arc<
    dyn Fn(HookInput, Option<String>, HookContext) -> BoxFuture<'static, Result<HookJsonOutput>>
        + Send
        + Sync,
>;

/// Hook matcher configuration.
#[derive(Clone, Default)]
pub struct HookMatcher {
    /// See <https://docs.anthropic.com/en/docs/claude-code/hooks#structure>
    /// for the expected string value. For example, for PreToolUse, the
    /// matcher can be a tool name like `"Bash"` or a combination of tool
    /// names like `"Write|MultiEdit|Edit"`.
    pub matcher: Option<String>,
    /// Callbacks invoked when the matcher matches.
    pub hooks: Vec<HookCallback>,
    /// Timeout in seconds for all hooks in this matcher (default: 60).
    pub timeout: Option<f64>,
}

impl std::fmt::Debug for HookMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookMatcher")
            .field("matcher", &self.matcher)
            .field("hooks", &format!("<{} callbacks>", self.hooks.len()))
            .field("timeout", &self.timeout)
            .finish()
    }
}
