//! Permission modes, permission updates, and tool-permission callback types.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::errors::Result;

/// Permission mode for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Standard permission behavior; prompts for dangerous operations.
    #[serde(rename = "default")]
    Default,
    /// Auto-accept file edit operations.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Planning mode, no execution of tools.
    #[serde(rename = "plan")]
    Plan,
    /// Bypass all permission checks.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// Don't prompt for permissions; deny if not pre-approved.
    #[serde(rename = "dontAsk")]
    DontAsk,
    /// A model classifier approves or denies each tool call.
    #[serde(rename = "auto")]
    Auto,
}

impl PermissionMode {
    /// The wire string for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
        }
    }
}

/// Where a [`PermissionUpdate`] is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionUpdateDestination {
    /// Global user settings.
    #[serde(rename = "userSettings")]
    UserSettings,
    /// Project settings.
    #[serde(rename = "projectSettings")]
    ProjectSettings,
    /// Local (gitignored) project settings.
    #[serde(rename = "localSettings")]
    LocalSettings,
    /// This session only.
    #[serde(rename = "session")]
    Session,
}

/// Behavior a permission rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Allow matching tool calls.
    Allow,
    /// Deny matching tool calls.
    Deny,
    /// Prompt for matching tool calls.
    Ask,
}

/// Permission rule value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Tool the rule applies to.
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// Optional rule specifier (e.g. `"ls:*"`), always present on the wire
    /// (as `null` when unset), matching the TypeScript control protocol.
    #[serde(rename = "ruleContent")]
    pub rule_content: Option<String>,
}

/// Permission update configuration.
///
/// Serializes to the control-protocol wire shape used by the TypeScript SDK
/// (a `type`-tagged object with camelCase rule fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PermissionUpdate {
    /// Add permission rules.
    #[serde(rename = "addRules")]
    AddRules {
        /// Rules to add.
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Behavior the rules apply.
        #[serde(skip_serializing_if = "Option::is_none")]
        behavior: Option<PermissionBehavior>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Replace permission rules.
    #[serde(rename = "replaceRules")]
    ReplaceRules {
        /// Replacement rules.
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Behavior the rules apply.
        #[serde(skip_serializing_if = "Option::is_none")]
        behavior: Option<PermissionBehavior>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Remove permission rules.
    #[serde(rename = "removeRules")]
    RemoveRules {
        /// Rules to remove.
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Behavior the rules apply.
        #[serde(skip_serializing_if = "Option::is_none")]
        behavior: Option<PermissionBehavior>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Change the permission mode.
    #[serde(rename = "setMode")]
    SetMode {
        /// The new mode.
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<PermissionMode>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Add accessible directories.
    #[serde(rename = "addDirectories")]
    AddDirectories {
        /// Directories to add.
        #[serde(skip_serializing_if = "Option::is_none")]
        directories: Option<Vec<String>>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Remove accessible directories.
    #[serde(rename = "removeDirectories")]
    RemoveDirectories {
        /// Directories to remove.
        #[serde(skip_serializing_if = "Option::is_none")]
        directories: Option<Vec<String>>,
        /// Where the update is persisted.
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
}

/// Context information for tool permission callbacks.
#[derive(Debug, Clone, Default)]
pub struct ToolPermissionContext {
    /// Permission suggestions from the CLI.
    pub suggestions: Vec<PermissionUpdate>,
    /// Unique identifier for this specific tool call within the assistant
    /// message. Multiple tool calls in the same assistant message will have
    /// different `tool_use_id`s.
    ///
    /// Always present when delivered to a `can_use_tool` callback (the wire
    /// protocol guarantees it); the `Option` only mirrors the Python SDK's
    /// field-ordering compatibility, so callers do not need to handle `None`.
    pub tool_use_id: Option<String>,
    /// If running within the context of a sub-agent, the sub-agent's ID.
    pub agent_id: Option<String>,
    /// The file path that triggered the permission request, if applicable.
    /// For example, when a Bash command tries to access a path outside
    /// allowed directories.
    pub blocked_path: Option<String>,
    /// Explains why this permission request was triggered. When a PreToolUse
    /// hook returns `permissionDecision: "ask"` with a
    /// `permissionDecisionReason`, that reason is forwarded here.
    pub decision_reason: Option<String>,
    /// Full permission prompt sentence (e.g. "Claude wants to read foo.txt").
    /// Use this as the primary prompt text when present instead of
    /// reconstructing from tool name + input.
    pub title: Option<String>,
    /// Short noun phrase for the tool action (e.g. "Read file"), suitable for
    /// button labels or compact UI.
    pub display_name: Option<String>,
    /// Human-readable subtitle for the permission UI.
    pub description: Option<String>,
}

/// Allow permission result.
#[derive(Debug, Clone, Default)]
pub struct PermissionResultAllow {
    /// Replacement tool input; the original input is used when `None`.
    pub updated_input: Option<Map<String, Value>>,
    /// Permission updates to apply alongside the allow.
    pub updated_permissions: Option<Vec<PermissionUpdate>>,
}

/// Deny permission result.
#[derive(Debug, Clone, Default)]
pub struct PermissionResultDeny {
    /// Message reported back to the model.
    pub message: String,
    /// Whether to interrupt the current turn.
    pub interrupt: bool,
}

/// Result of a tool permission callback.
#[derive(Debug, Clone)]
pub enum PermissionResult {
    /// Allow the tool call.
    Allow(PermissionResultAllow),
    /// Deny the tool call.
    Deny(PermissionResultDeny),
}

impl PermissionResult {
    /// Allow the tool call with its original input.
    pub fn allow() -> Self {
        Self::Allow(PermissionResultAllow::default())
    }

    /// Deny the tool call with a message for the model.
    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny(PermissionResultDeny {
            message: message.into(),
            interrupt: false,
        })
    }
}

/// Custom permission handler for tool calls that would otherwise prompt the
/// user. Receives `(tool_name, input, context)`.
pub type CanUseTool = Arc<
    dyn Fn(
            String,
            Map<String, Value>,
            ToolPermissionContext,
        ) -> BoxFuture<'static, Result<PermissionResult>>
        + Send
        + Sync,
>;
