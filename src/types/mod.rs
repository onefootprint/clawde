//! Type definitions for the Claude Agent SDK.

pub mod hooks;
pub mod mcp_config;
pub mod messages;
pub mod options;
pub mod permissions;
pub mod session;

pub use hooks::{
    AsyncHookJsonOutput, BaseHookInput, HookCallback, HookContext, HookEvent, HookInput,
    HookJsonOutput, HookMatcher, HookSpecificOutput, NotificationHookInput, PermissionDecision,
    PermissionRequestHookInput, PostToolUseFailureHookInput, PostToolUseHookInput,
    PreCompactHookInput, PreToolUseHookInput, StopHookInput, SubagentStartHookInput,
    SubagentStopHookInput, SyncHookJsonOutput, UserPromptSubmitHookInput,
};
pub use mcp_config::{
    McpHttpServerConfig, McpSdkServerConfig, McpServerConfig, McpServerConnectionStatus,
    McpServerInfo, McpServerStatus, McpServers, McpSseServerConfig, McpStatusResponse,
    McpStdioServerConfig, McpToolAnnotations, McpToolInfo,
};
pub use messages::{
    is_terminal_task_status, AssistantMessage, AssistantMessageError, ContentBlock,
    ContextUsageCategory, ContextUsageResponse, ConversationResetMessage, DeferredToolUse,
    HookEventMessage, Message, MessageOrigin, MessageOriginKind, MirrorErrorMessage, ModelUsage,
    RateLimitEvent, RateLimitInfo, RateLimitStatus, RateLimitType, ResultMessage, ServerToolName,
    ServerToolResultBlock, ServerToolUseBlock, StreamEvent, SystemMessage, TaskNotificationMessage,
    TaskNotificationOriginSubkind, TaskNotificationStatus, TaskProgressMessage, TaskStartedMessage,
    TaskUpdatedMessage, TaskUpdatedStatus, TaskUsage, TextBlock, ThinkingBlock, ToolResultBlock,
    ToolUseBlock, UserContent, UserMessage, TERMINAL_TASK_STATUSES,
};
pub use options::{
    AgentDefinition, AgentEffort, AgentMcpServerRef, ClaudeAgentOptions, EffortLevel,
    SandboxIgnoreViolations, SandboxNetworkConfig, SandboxSettings, SdkBeta, SdkPluginConfig,
    SettingSource, SkillsConfig, StderrCallback, SystemPrompt, TaskBudget, ThinkingConfig,
    ThinkingDisplay, ToolsConfig,
};
pub use permissions::{
    CanUseTool, PermissionBehavior, PermissionMode, PermissionResult, PermissionResultAllow,
    PermissionResultDeny, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
    ToolPermissionContext,
};
pub use session::{
    SdkSessionInfo, SessionKey, SessionListSubkeysKey, SessionMessage, SessionMessageType,
    SessionStore, SessionStoreEntry, SessionStoreFlushMode, SessionStoreListEntry,
    SessionStoreMethod, SessionSummaryEntry,
};
