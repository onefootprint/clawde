//! Content blocks and message types yielded by the SDK.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::session::SessionKey;

/// Text content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    /// The text.
    pub text: String,
}

/// Thinking content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// The (possibly summarized or empty) thinking text.
    pub thinking: String,
    /// Opaque signature for replay.
    pub signature: String,
}

/// Tool use content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Tool call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool input parameters.
    pub input: Value,
}

/// Tool result content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// The tool call this result answers.
    pub tool_use_id: String,
    /// Result content: a string or a list of content-block dicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// Whether the result is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Known server-side tool names. Branch on this to know which server tool was
/// invoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerToolName {
    /// The advisor tool.
    #[serde(rename = "advisor")]
    Advisor,
    /// Server-side web search.
    #[serde(rename = "web_search")]
    WebSearch,
    /// Server-side web fetch.
    #[serde(rename = "web_fetch")]
    WebFetch,
    /// Server-side code execution.
    #[serde(rename = "code_execution")]
    CodeExecution,
    /// Server-side bash code execution.
    #[serde(rename = "bash_code_execution")]
    BashCodeExecution,
    /// Server-side text-editor code execution.
    #[serde(rename = "text_editor_code_execution")]
    TextEditorCodeExecution,
    /// Regex tool search.
    #[serde(rename = "tool_search_tool_regex")]
    ToolSearchToolRegex,
    /// BM25 tool search.
    #[serde(rename = "tool_search_tool_bm25")]
    ToolSearchToolBm25,
    /// A server tool this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Server-side tool use block (e.g. advisor, web_search, web_fetch).
///
/// These are tools the API executes server-side on the model's behalf, so
/// they appear in the message stream alongside regular `tool_use` blocks but
/// the caller never needs to return a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    /// Tool call id.
    pub id: String,
    /// Which server tool was invoked.
    pub name: ServerToolName,
    /// Tool input parameters.
    pub input: Value,
}

/// Result block returned for a server-side tool call.
///
/// Mirrors [`ToolResultBlock`]'s shape. `content` is the raw object from the
/// API, opaque to this layer — callers that care about a specific server
/// tool's result schema can inspect `content["type"]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolResultBlock {
    /// The server tool call this result answers.
    pub tool_use_id: String,
    /// Raw result content.
    pub content: Value,
}

/// A content block within a message.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    /// Text content.
    Text(TextBlock),
    /// Thinking content.
    Thinking(ThinkingBlock),
    /// A tool call.
    ToolUse(ToolUseBlock),
    /// A tool result.
    ToolResult(ToolResultBlock),
    /// A server-side tool call.
    ServerToolUse(ServerToolUseBlock),
    /// A server-side tool result.
    ServerToolResult(ServerToolResultBlock),
}

/// Error classification on an [`AssistantMessage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantMessageError {
    /// Authentication failed.
    #[serde(rename = "authentication_failed")]
    AuthenticationFailed,
    /// Billing error.
    #[serde(rename = "billing_error")]
    BillingError,
    /// Rate limited.
    #[serde(rename = "rate_limit")]
    RateLimit,
    /// Invalid request.
    #[serde(rename = "invalid_request")]
    InvalidRequest,
    /// Server error.
    #[serde(rename = "server_error")]
    ServerError,
    /// Unknown error.
    #[serde(rename = "unknown")]
    Unknown,
    /// An error kind this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Known values of [`MessageOrigin::kind`]. Newer CLI versions may emit kinds
/// not listed here; treat anything unrecognized as "not human".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageOriginKind {
    /// A turn this application (or a human) submitted.
    #[serde(rename = "human")]
    Human,
    /// A message that arrived on an MCP channel.
    #[serde(rename = "channel")]
    Channel,
    /// A message from a peer session.
    #[serde(rename = "peer")]
    Peer,
    /// A background-task notification.
    #[serde(rename = "task-notification")]
    TaskNotification,
    /// A coordinator-injected turn.
    #[serde(rename = "coordinator")]
    Coordinator,
    /// Unclassified provenance.
    #[serde(rename = "unclassified")]
    Unclassified,
    /// A message from an observer.
    #[serde(rename = "observer")]
    Observer,
    /// An automatic continuation.
    #[serde(rename = "auto-continuation")]
    AutoContinuation,
    /// Observer activity.
    #[serde(rename = "observer-activity")]
    ObserverActivity,
    /// A kind this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Values of [`MessageOrigin::subkind`] for `kind == "task-notification"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskNotificationOriginSubkind {
    /// The fired prompt of a scheduled task.
    #[serde(rename = "scheduled-trigger")]
    ScheduledTrigger,
    /// A message sent from another of the user's sessions.
    #[serde(rename = "peer-send-message")]
    PeerSendMessage,
    /// A subkind this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Provenance of a user-role message, and — on a [`ResultMessage`] — of the
/// message that triggered that turn.
///
/// In streaming-input mode a single connection interleaves the turns you send
/// with turns the session injects on its own (background-task notifications,
/// scheduled-task prompts, MCP channel messages, messages relayed from peer
/// sessions, ...). `origin` tells them apart, e.g. to decide whether a
/// [`ResultMessage`] answers *your* prompt. Only `kind` is always present;
/// the remaining fields depend on `kind`. The object is passed through from
/// the CLI as-is and may carry additional undocumented keys in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageOrigin {
    /// Discriminator — see [`MessageOriginKind`].
    pub kind: MessageOriginKind,
    /// `kind == Channel`: name of the MCP server the message arrived on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// `kind == Peer` / `Observer`: sender address. Sender-asserted — use it
    /// for reply routing / display, never as proof of identity.
    #[serde(rename = "from", default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// `kind == Peer`: sender display name, already normalized by the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `kind == Peer`: the sender's host-openable session id, if its host
    /// provided one. A navigation target only.
    #[serde(
        rename = "fromSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub from_session: Option<String>,
    /// `kind == Peer` / `Observer`: task id of the in-process background
    /// subagent that sent this message. Absent for cross-session peers.
    #[serde(
        rename = "senderTaskId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sender_task_id: Option<String>,
    /// `kind == Peer`: decoded message body with the peer envelope stripped
    /// (byte-exact with what the model saw).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `kind == Peer`: kernel-verified pid of the process that connected to
    /// this session's local messaging socket. Absent when unverifiable.
    #[serde(
        rename = "verifiedPeerPid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub verified_peer_pid: Option<i64>,
    /// `kind == TaskNotification`: present when the delivery is the fired
    /// prompt of a scheduled task or a message sent from another session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<TaskNotificationOriginSubkind>,
    /// Fields not modeled by this SDK version.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl MessageOrigin {
    /// Whether this origin is the `human` kind.
    pub fn is_human(&self) -> bool {
        self.kind == MessageOriginKind::Human
    }
}

/// Content of a [`UserMessage`]: a plain string or a list of content blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum UserContent {
    /// A plain string prompt.
    Text(String),
    /// Structured content blocks.
    Blocks(Vec<ContentBlock>),
}

/// User message.
#[derive(Debug, Clone)]
pub struct UserMessage {
    /// Message content.
    pub content: UserContent,
    /// Unique id of the message, when reported.
    pub uuid: Option<String>,
    /// For subagent traffic, the id of the spawning Agent `tool_use`.
    pub parent_tool_use_id: Option<String>,
    /// Structured result payload for tool-result messages.
    pub tool_use_result: Option<Value>,
    /// Provenance of this message — see [`MessageOrigin`]. `None` when the
    /// CLI did not attribute it. Populated on injected turns (task
    /// notifications, channel/peer messages, ...) and on user messages the
    /// CLI replays; tool-result messages never carry it.
    pub origin: Option<MessageOrigin>,
}

/// Assistant message with content blocks.
#[derive(Debug, Clone)]
pub struct AssistantMessage {
    /// Content blocks.
    pub content: Vec<ContentBlock>,
    /// Model that produced the message.
    pub model: String,
    /// For subagent traffic, the id of the spawning Agent `tool_use`.
    pub parent_tool_use_id: Option<String>,
    /// Error classification, if the message reports one.
    pub error: Option<AssistantMessageError>,
    /// Raw API usage object.
    pub usage: Option<Value>,
    /// API message id.
    pub message_id: Option<String>,
    /// API stop reason.
    pub stop_reason: Option<String>,
    /// Session the message belongs to.
    pub session_id: Option<String>,
    /// Unique id of the message.
    pub uuid: Option<String>,
}

/// System message with metadata.
#[derive(Debug, Clone)]
pub struct SystemMessage {
    /// Message subtype.
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
}

/// Usage statistics reported in task_progress and task_notification messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskUsage {
    /// Total tokens spent.
    #[serde(default)]
    pub total_tokens: u64,
    /// Number of tool uses.
    #[serde(default)]
    pub tool_uses: u64,
    /// Task duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Possible status values for a `task_notification` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskNotificationStatus {
    /// Task completed.
    #[serde(rename = "completed")]
    Completed,
    /// Task failed.
    #[serde(rename = "failed")]
    Failed,
    /// Task was stopped.
    #[serde(rename = "stopped")]
    Stopped,
    /// A status this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Possible status values reported inside a `task_updated` patch.
/// `Pending`/`Running`/`Paused` are non-terminal; `Completed`/`Failed`/
/// `Killed` are terminal. Note `task_updated` reports the raw `killed`; the
/// CLI maps that to `stopped` only when it emits a `task_notification`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskUpdatedStatus {
    /// Task is pending.
    #[serde(rename = "pending")]
    Pending,
    /// Task is running.
    #[serde(rename = "running")]
    Running,
    /// Task is paused.
    #[serde(rename = "paused")]
    Paused,
    /// Task completed.
    #[serde(rename = "completed")]
    Completed,
    /// Task failed.
    #[serde(rename = "failed")]
    Failed,
    /// Task was killed.
    #[serde(rename = "killed")]
    Killed,
    /// A status this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Task statuses that mean the task has finished and should be cleared from
/// any "active task" tracking. This set spans both lifecycle vocabularies:
/// `task_notification` reports `stopped` (the CLI's mapped form of a killed
/// task) while `task_updated` reports the raw `killed`.
pub const TERMINAL_TASK_STATUSES: [&str; 4] = ["completed", "failed", "stopped", "killed"];

/// Whether `status` is a terminal task status (see
/// [`TERMINAL_TASK_STATUSES`]).
pub fn is_terminal_task_status(status: &str) -> bool {
    TERMINAL_TASK_STATUSES.contains(&status)
}

/// System message emitted when a task starts.
#[derive(Debug, Clone)]
pub struct TaskStartedMessage {
    /// Message subtype (`"task_started"`).
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
    /// Task id.
    pub task_id: String,
    /// Task description.
    pub description: String,
    /// Unique id of the message.
    pub uuid: String,
    /// Session the message belongs to.
    pub session_id: String,
    /// Spawning tool use id, if any.
    pub tool_use_id: Option<String>,
    /// Task type (e.g. `"local_agent"`).
    pub task_type: Option<String>,
}

/// System message emitted while a task is in progress.
#[derive(Debug, Clone)]
pub struct TaskProgressMessage {
    /// Message subtype (`"task_progress"`).
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
    /// Task id.
    pub task_id: String,
    /// Task description.
    pub description: String,
    /// Usage so far.
    pub usage: TaskUsage,
    /// Unique id of the message.
    pub uuid: String,
    /// Session the message belongs to.
    pub session_id: String,
    /// Spawning tool use id, if any.
    pub tool_use_id: Option<String>,
    /// Name of the last tool the task used.
    pub last_tool_name: Option<String>,
}

/// System message emitted when a task completes, fails, or is stopped.
///
/// Note: not every terminal task emits this message. Background tasks may
/// instead report completion only via a [`TaskUpdatedMessage`] whose
/// `patch.status` is terminal (see [`TERMINAL_TASK_STATUSES`]). Consumers
/// tracking active task IDs should clear them on a terminal status from
/// *either* message.
#[derive(Debug, Clone)]
pub struct TaskNotificationMessage {
    /// Message subtype (`"task_notification"`).
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
    /// Task id.
    pub task_id: String,
    /// Terminal status.
    pub status: TaskNotificationStatus,
    /// Path to the task's output file.
    pub output_file: String,
    /// Task summary.
    pub summary: String,
    /// Unique id of the message.
    pub uuid: String,
    /// Session the message belongs to.
    pub session_id: String,
    /// Spawning tool use id, if any.
    pub tool_use_id: Option<String>,
    /// Final usage, if reported.
    pub usage: Option<TaskUsage>,
}

/// System message emitted when a background task's state changes.
///
/// The CLI emits `system`/`task_updated` events as a task moves through its
/// lifecycle. `patch` carries the changed fields (e.g. `status`, `end_time`);
/// when `patch.status` is terminal (see [`TERMINAL_TASK_STATUSES`]) the task
/// has finished. A background task's terminal state can arrive *only* as a
/// [`TaskUpdatedMessage`] with no accompanying [`TaskNotificationMessage`].
#[derive(Debug, Clone)]
pub struct TaskUpdatedMessage {
    /// Message subtype (`"task_updated"`).
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
    /// Task id.
    pub task_id: String,
    /// The changed fields.
    pub patch: Map<String, Value>,
    /// The patch's status, if present.
    pub status: Option<TaskUpdatedStatus>,
    /// Session the message belongs to.
    pub session_id: Option<String>,
    /// Unique id of the message.
    pub uuid: Option<String>,
}

/// System message emitted when a [`crate::SessionStore::append`] call fails.
///
/// Non-fatal — the local-disk transcript is already durable, so the session
/// continues unaffected. The mirrored copy in the external store will be
/// missing the failed batch.
#[derive(Debug, Clone)]
pub struct MirrorErrorMessage {
    /// Message subtype (`"mirror_error"`).
    pub subtype: String,
    /// Full raw payload.
    pub data: Value,
    /// Key of the failed append, if known.
    pub key: Option<SessionKey>,
    /// Description of the failure.
    pub error: String,
}

/// Hook event emitted by the CLI when
/// [`crate::ClaudeAgentOptions::include_hook_events`] is enabled.
///
/// These arrive on the wire as `{"type": "system", "subtype":
/// "hook_started" | "hook_response", "hook_event": "PreToolUse", ...}`.
#[derive(Debug, Clone)]
pub struct HookEventMessage {
    /// Lifecycle phase — `"hook_started"` when a hook begins executing,
    /// `"hook_response"` when it completes (the latter carries `output`,
    /// `exit_code`, and `outcome` keys in `data`).
    pub subtype: String,
    /// Name of the hook event (e.g. `"PreToolUse"`).
    pub hook_event_name: String,
    /// Full raw event payload, including any event-specific fields not
    /// modeled here.
    pub data: Value,
    /// Session the event belongs to, if present.
    pub session_id: Option<String>,
    /// Unique id of the event, if present.
    pub uuid: Option<String>,
}

/// Tool use that was deferred by a PreToolUse hook returning `"defer"`.
///
/// When a PreToolUse hook returns `permissionDecision: "defer"`, the run
/// stops and the result message carries the deferred tool call here so the
/// caller can inspect it and decide whether to resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredToolUse {
    /// Tool call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool input parameters.
    pub input: Value,
}

/// Per-model token usage and cost breakdown.
///
/// Field names match the TypeScript SDK's `ModelUsage` shape (camelCase),
/// since the value is passed through verbatim from the CLI's `modelUsage`
/// field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Input tokens.
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: u64,
    /// Cache-read input tokens.
    #[serde(rename = "cacheReadInputTokens", default)]
    pub cache_read_input_tokens: u64,
    /// Cache-creation input tokens.
    #[serde(rename = "cacheCreationInputTokens", default)]
    pub cache_creation_input_tokens: u64,
    /// Web search requests.
    #[serde(rename = "webSearchRequests", default)]
    pub web_search_requests: u64,
    /// Cost in USD.
    #[serde(rename = "costUSD", default)]
    pub cost_usd: f64,
    /// Model context window.
    #[serde(rename = "contextWindow", default)]
    pub context_window: u64,
    /// Model max output tokens.
    #[serde(rename = "maxOutputTokens", default)]
    pub max_output_tokens: u64,
    /// Canonical model id used for the pricing lookup. May differ from the
    /// raw model string this entry is keyed by.
    #[serde(
        rename = "canonicalModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub canonical_model: Option<String>,
    /// API provider that served this model (`"firstParty"`, `"bedrock"`,
    /// `"vertex"`, `"foundry"`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Fields not modeled by this SDK version.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Result message with cost and usage information.
#[derive(Debug, Clone)]
pub struct ResultMessage {
    /// Result subtype (e.g. `"success"`, `"error_max_turns"`).
    pub subtype: String,
    /// Total wall-clock duration in milliseconds.
    pub duration_ms: i64,
    /// API duration in milliseconds.
    pub duration_api_ms: i64,
    /// Whether the run ended in error.
    pub is_error: bool,
    /// Number of conversation turns.
    pub num_turns: i64,
    /// Session the result belongs to.
    pub session_id: String,
    /// API stop reason, if any.
    pub stop_reason: Option<String>,
    /// Total cost in USD.
    pub total_cost_usd: Option<f64>,
    /// Raw API usage object.
    pub usage: Option<Value>,
    /// Result text, if any.
    pub result: Option<String>,
    /// Structured output, when `output_format` was configured.
    pub structured_output: Option<Value>,
    /// Per-model usage breakdown (from the CLI's `modelUsage` field).
    pub model_usage: Option<std::collections::HashMap<String, ModelUsage>>,
    /// Permission denials during the run.
    pub permission_denials: Option<Vec<Value>>,
    /// A tool call deferred by a PreToolUse hook, if the run stopped on one.
    pub deferred_tool_use: Option<DeferredToolUse>,
    /// Error strings reported by the CLI.
    pub errors: Option<Vec<String>>,
    /// HTTP status code (e.g. 429, 500, 529) of the failing API call when
    /// `is_error` is true and `subtype` is `"success"`; `None` otherwise.
    /// Emitted by the CLI since v2.1.110. Safe to log (no message content).
    pub api_error_status: Option<i64>,
    /// Unique id of the message.
    pub uuid: Option<String>,
    /// Why the query loop terminated (e.g. `"completed"`, `"max_turns"`,
    /// `"aborted_streaming"`). A value of `"aborted_streaming"` or
    /// `"aborted_tools"` indicates the turn was cancelled. `None` when the
    /// CLI did not report a terminal reason.
    pub terminal_reason: Option<String>,
    /// Origin of the user message that triggered this turn — see
    /// [`MessageOrigin`]. Lets a streaming-input consumer distinguish the
    /// result of its own prompt (`None`, or `kind == Human` if it stamped
    /// that) from results of injected turns such as background-task
    /// notifications.
    pub origin: Option<MessageOrigin>,
}

/// Stream event for partial message updates during streaming.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    /// Unique id of the event.
    pub uuid: String,
    /// Session the event belongs to.
    pub session_id: String,
    /// The raw Anthropic API stream event.
    pub event: Value,
    /// For subagent traffic, the id of the spawning Agent `tool_use`.
    pub parent_tool_use_id: Option<String>,
}

/// Rate limit status values — see
/// <https://docs.claude.com/en/docs/claude-code/rate-limits>.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitStatus {
    /// Under the limit.
    #[serde(rename = "allowed")]
    Allowed,
    /// Approaching the limit.
    #[serde(rename = "allowed_warning")]
    AllowedWarning,
    /// The limit has been hit.
    #[serde(rename = "rejected")]
    Rejected,
    /// A status this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Which rate limit window applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitType {
    /// Five-hour window.
    #[serde(rename = "five_hour")]
    FiveHour,
    /// Seven-day window.
    #[serde(rename = "seven_day")]
    SevenDay,
    /// Seven-day Opus window.
    #[serde(rename = "seven_day_opus")]
    SevenDayOpus,
    /// Seven-day Sonnet window.
    #[serde(rename = "seven_day_sonnet")]
    SevenDaySonnet,
    /// Overage window.
    #[serde(rename = "overage")]
    Overage,
    /// A type this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Rate limit status emitted by the CLI when rate limit state changes.
#[derive(Debug, Clone)]
pub struct RateLimitInfo {
    /// Current rate limit status. `AllowedWarning` means approaching the
    /// limit; `Rejected` means the limit has been hit.
    pub status: RateLimitStatus,
    /// Unix timestamp when the rate limit window resets.
    pub resets_at: Option<i64>,
    /// Which rate limit window applies.
    pub rate_limit_type: Option<RateLimitType>,
    /// Fraction of the rate limit consumed (0.0 - 1.0).
    pub utilization: Option<f64>,
    /// Status of overage/pay-as-you-go usage if applicable.
    pub overage_status: Option<RateLimitStatus>,
    /// Unix timestamp when the overage window resets.
    pub overage_resets_at: Option<i64>,
    /// Why overage is unavailable if status is rejected.
    pub overage_disabled_reason: Option<String>,
    /// Full raw object from the CLI, including any fields not modeled above.
    pub raw: Value,
}

/// Rate limit event emitted when rate limit info changes.
///
/// The CLI emits this whenever the rate limit status transitions (e.g. from
/// `allowed` to `allowed_warning`). Use this to warn users before they hit a
/// hard limit, or to gracefully back off when the status is `Rejected`.
#[derive(Debug, Clone)]
pub struct RateLimitEvent {
    /// The new rate limit state.
    pub rate_limit_info: RateLimitInfo,
    /// Unique id of the event.
    pub uuid: String,
    /// Session the event belongs to.
    pub session_id: String,
}

/// Emitted when the session's conversation is replaced without ending the
/// connection — e.g. after `/clear` or any other flow that discards the
/// transcript mid-session.
///
/// In streaming input mode a single connection can carry many user turns, and
/// a reset clears the conversation history *and* zeroes the running totals
/// reported on subsequent [`ResultMessage`]s (e.g. `total_cost_usd`). If you
/// accumulate those totals across a long-lived session, snapshot them when
/// this message arrives.
#[derive(Debug, Clone)]
pub struct ConversationResetMessage {
    /// Opaque identifier for the fresh conversation, for UIs to key an empty
    /// transcript on. This is *not* the `session_id` of subsequent messages —
    /// read that from the next message.
    pub new_conversation_id: String,
    /// Unique id of this message.
    pub uuid: String,
    /// ID of the session that was reset (the outgoing session; messages
    /// after the reset carry a new `session_id`).
    pub session_id: String,
}

/// A message from the conversation.
///
/// The Python SDK models the task/mirror/hook messages as `SystemMessage`
/// subclasses; here they are sibling variants — match on
/// [`Message::System`] *and* the task variants where Python code would
/// `isinstance(msg, SystemMessage)`.
#[derive(Debug, Clone)]
pub enum Message {
    /// A user message.
    User(UserMessage),
    /// An assistant message.
    Assistant(AssistantMessage),
    /// A system message.
    System(SystemMessage),
    /// A task started.
    TaskStarted(TaskStartedMessage),
    /// Task progress.
    TaskProgress(TaskProgressMessage),
    /// A task reached a terminal state.
    TaskNotification(TaskNotificationMessage),
    /// A background task's state changed.
    TaskUpdated(TaskUpdatedMessage),
    /// A session-store mirror append failed.
    MirrorError(MirrorErrorMessage),
    /// A hook lifecycle event.
    HookEvent(HookEventMessage),
    /// The result of a turn. Boxed to keep the enum small; a result arrives
    /// once per turn.
    Result(Box<ResultMessage>),
    /// A partial-message stream event.
    Stream(StreamEvent),
    /// A rate limit transition.
    RateLimit(RateLimitEvent),
    /// The conversation was reset.
    ConversationReset(ConversationResetMessage),
}

/// A single context usage category (system prompt, tools, messages, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsageCategory {
    /// Category name.
    pub name: String,
    /// Tokens in this category.
    #[serde(default)]
    pub tokens: i64,
    /// Display color.
    #[serde(default)]
    pub color: String,
    /// Whether the category is deferred.
    #[serde(
        rename = "isDeferred",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub is_deferred: Option<bool>,
}

/// Response from [`crate::ClaudeSdkClient::get_context_usage`].
///
/// Provides a breakdown of current context window usage by category, matching
/// the data shown by the `/context` command in the CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextUsageResponse {
    /// Token usage broken down by category.
    #[serde(default)]
    pub categories: Vec<ContextUsageCategory>,
    /// Total tokens currently in the context window.
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: i64,
    /// Effective maximum tokens (may be reduced by autocompact buffer).
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: i64,
    /// Raw model context window size.
    #[serde(rename = "rawMaxTokens", default)]
    pub raw_max_tokens: i64,
    /// Percentage of context window used (0-100).
    #[serde(default)]
    pub percentage: f64,
    /// Model name the context usage is calculated for.
    #[serde(default)]
    pub model: String,
    /// Whether autocompact is enabled for this session.
    #[serde(rename = "isAutoCompactEnabled", default)]
    pub is_auto_compact_enabled: bool,
    /// CLAUDE.md and memory files loaded, with path, type, and token counts.
    #[serde(rename = "memoryFiles", default)]
    pub memory_files: Vec<Value>,
    /// MCP tools with name, serverName, tokens, and isLoaded status.
    #[serde(rename = "mcpTools", default)]
    pub mcp_tools: Vec<Value>,
    /// Agent definitions with agentType, source, and token counts.
    #[serde(default)]
    pub agents: Vec<Value>,
    /// Visual grid representation used by the CLI context display.
    #[serde(rename = "gridRows", default)]
    pub grid_rows: Vec<Value>,
    /// Token threshold at which autocompact triggers.
    #[serde(
        rename = "autoCompactThreshold",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_compact_threshold: Option<i64>,
    /// Built-in tools deferred from the initial tool list.
    #[serde(
        rename = "deferredBuiltinTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub deferred_builtin_tools: Option<Vec<Value>>,
    /// System (built-in) tools with name and token counts.
    #[serde(
        rename = "systemTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_tools: Option<Vec<Value>>,
    /// System prompt sections with name and token counts.
    #[serde(
        rename = "systemPromptSections",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt_sections: Option<Vec<Value>>,
    /// Slash command usage summary.
    #[serde(
        rename = "slashCommands",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub slash_commands: Option<Value>,
    /// Skill usage summary with frontmatter breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    /// Detailed breakdown of message tokens by type.
    #[serde(
        rename = "messageBreakdown",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub message_breakdown: Option<Value>,
    /// Cumulative API usage for the session.
    #[serde(rename = "apiUsage", default, skip_serializing_if = "Option::is_none")]
    pub api_usage: Option<Value>,
    /// Fields not modeled by this SDK version.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
