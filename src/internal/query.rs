//! Bidirectional control protocol on top of [`Transport`].
//!
//! Manages control request/response routing, hook callbacks, tool permission
//! callbacks, SDK MCP message routing, message streaming, and the
//! initialization handshake.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;

use crate::errors::{normalize_result_errors, ClaudeSdkError, Result};
use crate::internal::transcript_mirror_batcher::TranscriptMirrorBatcher;
use crate::mcp::SdkMcpServer;
use crate::transport::Transport;
use crate::types::hooks::{HookContext, HookInput};
use crate::types::messages::is_terminal_task_status;
use crate::types::permissions::{PermissionResult, PermissionUpdate, ToolPermissionContext};
use crate::types::{HookCallback, HookEvent, HookMatcher, PermissionMode, SessionKey};

/// Task types whose completion runs a follow-up turn, and which therefore may
/// still need the control channel after the turn's result frame.
///
/// This mirrors the set the CLI itself holds a result back for, which is
/// narrower than its notion of "delegated agent work": background shells and
/// monitors run indefinitely by design, teammates are long-lived, and remote
/// agents can be long-running monitors — deferring the stdin close on any of
/// them would withhold it forever rather than briefly. Anything added here
/// must be a type that reliably reaches a terminal status, or it will hang
/// the query.
const DEFERRING_TASK_TYPES: [&str; 2] = ["local_agent", "local_workflow"];

/// Default timeout for control requests.
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long shutdown waits for an SDK MCP server's `close()` before giving
/// up on it (mirrors the Python bridge's shutdown grace period).
const SDK_MCP_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Pick the most informative text from a `result` frame with `is_error`.
///
/// Terminal errors the CLI raises itself (`error_max_turns`,
/// `error_during_execution`, ...) carry their prose in `errors[]`. A run that
/// ends on an API failure instead arrives as `subtype: "success"` with
/// `is_error: true`, an empty `errors[]` and the "API Error: ..." prose in
/// `result`. Prefer `errors[]`, then `result`, then a non-success `subtype`,
/// then the HTTP status, mirroring the TypeScript SDK.
fn error_result_text(message: &Value) -> String {
    let errors = normalize_result_errors(message.get("errors"));
    if !errors.is_empty() {
        return errors.join("; ");
    }
    if let Some(result) = message.get("result").and_then(Value::as_str) {
        let trimmed = result.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(subtype) = message.get("subtype").and_then(Value::as_str) {
        if !subtype.is_empty() && subtype != "success" {
            return subtype.to_string();
        }
    }
    if let Some(status) = message.get("api_error_status") {
        if !status.is_null() {
            return format!("API error (HTTP {status})");
        }
    }
    "unknown error".to_string()
}

/// One item on the consumer-facing message channel.
pub(crate) enum QueryMessage {
    /// A regular SDK message.
    Data(Value),
    /// A fatal error; the stream ends after yielding it.
    Error(ClaudeSdkError),
    /// End of stream.
    End,
}

struct QueryState {
    pending_control: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    hook_callbacks: Mutex<HashMap<String, HookCallback>>,
    next_callback_id: AtomicU64,
    request_counter: AtomicU64,
    read_task: Mutex<Option<JoinHandle<()>>>,
    child_tasks: Mutex<Vec<JoinHandle<()>>>,
    inflight_requests: Mutex<HashMap<String, JoinHandle<()>>>,
    initialized: AtomicBool,
    closed: AtomicBool,
    initialization_result: Mutex<Option<Value>>,
    // Set when a run-ending result arrives (a result frame with no tasks in
    // flight) so the stdin-closing waiter can wake. Named for history — it
    // once tracked the literal first result.
    first_result_tx: watch::Sender<bool>,
    // Task IDs of started-but-not-finished tasks. A result frame only ends
    // one turn, not the run: background tasks keep running past it and still
    // need stdin for hook/SDK-MCP control responses, so a result that
    // arrives while this set is non-empty must not close stdin.
    inflight_tasks: Mutex<HashSet<String>>,
    // Set to the result payload when the most recent message is a result
    // with is_error=true. Used to replace the generic "exit code 1" process
    // error with a ResultError carrying what the CLI already reported.
    last_error_result: Mutex<Option<Value>>,
    mirror_batcher: Mutex<Option<Arc<TranscriptMirrorBatcher>>>,
}

/// Handles the bidirectional control protocol on top of a [`Transport`].
pub(crate) struct Query {
    pub(crate) transport: Arc<dyn Transport>,
    is_streaming_mode: bool,
    can_use_tool: Option<crate::types::CanUseTool>,
    hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,
    sdk_mcp_servers: HashMap<String, Arc<dyn SdkMcpServer>>,
    initialize_timeout: Duration,
    agents: Option<Value>,
    exclude_dynamic_sections: Option<bool>,
    skills: Option<Vec<String>>,
    forward_subagent_text: bool,
    message_tx: mpsc::Sender<QueryMessage>,
    message_rx: Mutex<Option<mpsc::Receiver<QueryMessage>>>,
    state: QueryState,
}

pub(crate) struct QueryOptions {
    pub can_use_tool: Option<crate::types::CanUseTool>,
    pub hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,
    pub sdk_mcp_servers: HashMap<String, Arc<dyn SdkMcpServer>>,
    pub initialize_timeout: Duration,
    pub agents: Option<Value>,
    pub exclude_dynamic_sections: Option<bool>,
    pub skills: Option<Vec<String>>,
    pub forward_subagent_text: bool,
}

impl Query {
    pub(crate) fn new(transport: Arc<dyn Transport>, options: QueryOptions) -> Arc<Self> {
        let (message_tx, message_rx) = mpsc::channel(100);
        let (first_result_tx, _) = watch::channel(false);
        Arc::new(Self {
            transport,
            is_streaming_mode: true,
            can_use_tool: options.can_use_tool,
            hooks: options.hooks,
            sdk_mcp_servers: options.sdk_mcp_servers,
            initialize_timeout: options.initialize_timeout,
            agents: options.agents,
            exclude_dynamic_sections: options.exclude_dynamic_sections,
            skills: options.skills,
            forward_subagent_text: options.forward_subagent_text,
            message_tx,
            message_rx: Mutex::new(Some(message_rx)),
            state: QueryState {
                pending_control: Mutex::new(HashMap::new()),
                hook_callbacks: Mutex::new(HashMap::new()),
                next_callback_id: AtomicU64::new(0),
                request_counter: AtomicU64::new(0),
                read_task: Mutex::new(None),
                child_tasks: Mutex::new(Vec::new()),
                inflight_requests: Mutex::new(HashMap::new()),
                initialized: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                initialization_result: Mutex::new(None),
                first_result_tx,
                inflight_tasks: Mutex::new(HashSet::new()),
                last_error_result: Mutex::new(None),
                mirror_batcher: Mutex::new(None),
            },
        })
    }

    /// Attach a batcher that receives `transcript_mirror` frames.
    ///
    /// When set, the read loop peels `transcript_mirror` frames off stdout
    /// (they are not yielded to consumers), enqueues them on the batcher, and
    /// flushes before yielding each `result` message.
    pub(crate) async fn set_transcript_mirror_batcher(
        &self,
        batcher: Arc<TranscriptMirrorBatcher>,
    ) {
        *self.state.mirror_batcher.lock().await = Some(batcher);
    }

    /// Surface a [`crate::SessionStore::append`] failure as a system message.
    ///
    /// Called from the batcher's `on_error`; the dropped batch is not retried
    /// (at-most-once delivery), so this is the consumer's only signal.
    /// Non-blocking — if the message buffer is full the error is logged and
    /// dropped rather than back-pressuring the read loop.
    pub(crate) fn report_mirror_error(&self, key: Option<&SessionKey>, error: &str) {
        let msg = json!({
            "type": "system",
            "subtype": "mirror_error",
            "error": error,
            "key": key.map(|k| serde_json::to_value(k).unwrap_or(Value::Null)),
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": key.map(|k| k.session_id.clone()).unwrap_or_default(),
        });
        if let Err(e) = self.message_tx.try_send(QueryMessage::Data(msg)) {
            tracing::warn!(
                target: "clawde",
                "Dropping mirror_error message (buffer full): {e}"
            );
        }
    }

    /// Initialize the control protocol.
    ///
    /// Returns the initialize response with supported commands.
    pub(crate) async fn initialize(&self) -> Result<Option<Value>> {
        if !self.is_streaming_mode {
            return Ok(None);
        }

        // Build the hooks configuration, registering callback ids.
        let mut hooks_config = Map::new();
        if let Some(hooks) = &self.hooks {
            let mut callbacks = self.state.hook_callbacks.lock().await;
            for (event, matchers) in hooks {
                if matchers.is_empty() {
                    continue;
                }
                let mut matcher_configs = Vec::new();
                for matcher in matchers {
                    let mut callback_ids = Vec::new();
                    for callback in &matcher.hooks {
                        let id = format!(
                            "hook_{}",
                            self.state.next_callback_id.fetch_add(1, Ordering::SeqCst)
                        );
                        callbacks.insert(id.clone(), callback.clone());
                        callback_ids.push(id);
                    }
                    let mut config = json!({
                        "matcher": matcher.matcher,
                        "hookCallbackIds": callback_ids,
                    });
                    if let Some(timeout) = matcher.timeout {
                        config["timeout"] = json!(timeout);
                    }
                    matcher_configs.push(config);
                }
                hooks_config.insert(event.as_str().to_string(), Value::Array(matcher_configs));
            }
        }

        let mut request = json!({
            "subtype": "initialize",
            "hooks": if hooks_config.is_empty() { Value::Null } else { Value::Object(hooks_config) },
        });
        if let Some(agents) = &self.agents {
            request["agents"] = agents.clone();
        }
        if let Some(eds) = self.exclude_dynamic_sections {
            request["excludeDynamicSections"] = json!(eds);
        }
        // "all" and omitted are equivalent at the wire level (no filter), so
        // the field is only sent when it's an explicit list.
        if let Some(skills) = &self.skills {
            request["skills"] = json!(skills);
        }
        if self.forward_subagent_text {
            request["forwardSubagentText"] = json!(true);
        }

        // Longer timeout for initialize since MCP servers may take time to
        // start.
        let response = self
            .send_control_request(request, self.initialize_timeout)
            .await?;
        self.state.initialized.store(true, Ordering::SeqCst);
        *self.state.initialization_result.lock().await = Some(response.clone());
        Ok(Some(response))
    }

    /// Start reading messages from the transport.
    pub(crate) async fn start(self: &Arc<Self>) {
        let mut read_task = self.state.read_task.lock().await;
        if read_task.is_none() {
            let this = self.clone();
            *read_task = Some(tokio::spawn(async move { this.read_messages_loop().await }));
        }
    }

    /// Spawn a child task that will be aborted on close().
    pub(crate) async fn spawn_task(
        self: &Arc<Self>,
        fut: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let handle = tokio::spawn(fut);
        let mut child_tasks = self.state.child_tasks.lock().await;
        child_tasks.retain(|t| !t.is_finished());
        child_tasks.push(handle);
    }

    /// The initialize response, if the handshake has completed.
    pub(crate) async fn initialization_result(&self) -> Option<Value> {
        self.state.initialization_result.lock().await.clone()
    }

    async fn read_messages_loop(self: Arc<Self>) {
        let mut stream = self.transport.read_messages();
        let mut pending_error: Option<ClaudeSdkError> = None;

        while let Some(item) = stream.next().await {
            if self.state.closed.load(Ordering::SeqCst) {
                break;
            }
            let message = match item {
                Ok(message) => message,
                Err(e) => {
                    // When the CLI emits a result with is_error=true it then
                    // exits non-zero on purpose, for shell-script consumers.
                    // The trailing process error carries no information
                    // beyond "exit code 1" — replace it with a ResultError
                    // carrying what the CLI already reported so the error is
                    // actionable and typed. Mirrors the TypeScript SDK.
                    let last_error_result = self.state.last_error_result.lock().await.clone();
                    let error = match (&e, last_error_result) {
                        (ClaudeSdkError::Process { exit_code, .. }, Some(result_data)) => {
                            let text = format!(
                                "Claude Code returned an error result: {}",
                                error_result_text(&result_data)
                            );
                            tracing::debug!(
                                target: "clawde",
                                "Replacing process error (exit code {exit_code:?}) with ResultError"
                            );
                            ClaudeSdkError::ResultError {
                                message: text,
                                exit_code: *exit_code,
                                data: result_data,
                            }
                        }
                        _ => {
                            tracing::error!(
                                target: "clawde",
                                "Fatal error in message reader: {e}"
                            );
                            e
                        }
                    };
                    pending_error = Some(error);
                    break;
                }
            };

            let msg_type = message.get("type").and_then(Value::as_str).unwrap_or("");

            match msg_type {
                "control_response" => {
                    let response = message.get("response").cloned().unwrap_or(json!({}));
                    let request_id = response
                        .get("request_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let mut pending = self.state.pending_control.lock().await;
                    if let Some(sender) = pending.remove(&request_id) {
                        let outcome =
                            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                                Err(response
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Unknown error")
                                    .to_string())
                            } else {
                                Ok(response)
                            };
                        let _ = sender.send(outcome);
                    }
                    continue;
                }
                "control_request" => {
                    if !self.state.closed.load(Ordering::SeqCst) {
                        self.spawn_control_request_handler(message).await;
                    }
                    continue;
                }
                "control_cancel_request" => {
                    if let Some(cancel_id) = message.get("request_id").and_then(Value::as_str) {
                        if let Some(handle) =
                            self.state.inflight_requests.lock().await.remove(cancel_id)
                        {
                            // The CLI has already abandoned this request;
                            // abort the handler so no response is written.
                            handle.abort();
                        }
                    }
                    continue;
                }
                "transcript_mirror" => {
                    // SessionStore write path: peel mirror frames off stdout
                    // and hand them to the batcher; do NOT yield to
                    // consumers.
                    if let Some(batcher) = self.state.mirror_batcher.lock().await.as_ref() {
                        let file_path = message
                            .get("filePath")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let entries = message
                            .get("entries")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        batcher.enqueue(file_path, entries);
                    }
                    continue;
                }
                _ => {}
            }

            // Track task lifecycle frames so results can tell "one turn
            // ended" apart from "the run is done".
            if msg_type == "system" {
                self.track_task_lifecycle(&message).await;
            }

            if msg_type == "result" {
                // Flush pending transcript mirror entries before yielding
                // the result so consumers observing it can rely on the
                // SessionStore being up to date for this turn.
                if let Some(batcher) = self.state.mirror_batcher.lock().await.as_ref() {
                    batcher.flush().await;
                }
                let inflight = self.state.inflight_tasks.lock().await.len();
                if inflight > 0 {
                    // One turn ended, but background tasks are still running
                    // and may need hook/SDK-MCP control responses over
                    // stdin. Closing it now silently disables hooks and
                    // fails SDK-MCP calls. Each task completion wakes the
                    // parent for a follow-up turn, so a later result frame
                    // arrives with no tasks in flight and closes stdin then.
                    tracing::debug!(
                        target: "clawde",
                        "Result received with {inflight} task(s) in flight; keeping stdin open"
                    );
                } else {
                    let _ = self.state.first_result_tx.send(true);
                }
                *self.state.last_error_result.lock().await =
                    if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                        Some(message.clone())
                    } else {
                        None
                    };
            } else if !(msg_type == "system"
                && message.get("subtype").and_then(Value::as_str) == Some("session_state_changed"))
            {
                // Anything other than the post-turn session_state_changed
                // marker means the conversation moved on; a process error now
                // is a fresh crash, not the expected exit from a prior error
                // result. Mirrors the TypeScript SDK's reset logic.
                *self.state.last_error_result.lock().await = None;
            }

            // Regular SDK messages go to the stream.
            if self
                .message_tx
                .send(QueryMessage::Data(message))
                .await
                .is_err()
            {
                break;
            }
        }

        // Signal all pending control requests so they fail fast instead of
        // timing out. This includes an `initialize` still in flight when the
        // CLI reports an error result during startup (e.g. a refused resume),
        // so that path sees the same actionable text.
        if let Some(error) = &pending_error {
            let error_text = error.to_string();
            let mut pending = self.state.pending_control.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(error_text.clone()));
            }
        }

        // Flush any remaining transcript mirror entries before closing so an
        // early stdout EOF or transport error doesn't drop entries batched
        // this turn.
        if let Some(batcher) = self.state.mirror_batcher.lock().await.as_ref() {
            batcher.flush().await;
        }
        // Unblock any waiters (e.g. string-prompt path waiting for first
        // result) so they don't stall for the full timeout on early exit.
        let _ = self.state.first_result_tx.send(true);
        // Signal end of stream. The typed error rides along so consumers
        // re-raise it as-is instead of flattening it to a string.
        if let Some(error) = pending_error {
            let _ = self.message_tx.try_send(QueryMessage::Error(error));
        }
        let _ = self.message_tx.try_send(QueryMessage::End);
    }

    async fn spawn_control_request_handler(self: &Arc<Self>, request: Value) {
        let request_id = request
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let this = self.clone();
        let req_id = request_id.clone();
        let handle = tokio::spawn(async move {
            this.handle_control_request(request).await;
            this.state.inflight_requests.lock().await.remove(&req_id);
        });
        self.state
            .inflight_requests
            .lock()
            .await
            .insert(request_id, handle);
    }

    async fn handle_control_request(self: &Arc<Self>, request: Value) {
        let request_id = request
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let request_data = request.get("request").cloned().unwrap_or(json!({}));
        let subtype = request_data
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Isolated against panics in user-supplied callbacks (can_use_tool,
        // hooks, SDK MCP handlers): the CLI blocks until it gets a matching
        // control_response, so a panic that unwound this task without one
        // would hang that request CLI-side until its timeout. Python gets
        // the same guarantee from its blanket exception handler.
        let dispatch =
            std::panic::AssertUnwindSafe(self.dispatch_control_request(&subtype, &request_data));
        let outcome = match futures::FutureExt::catch_unwind(dispatch).await {
            Ok(outcome) => outcome,
            Err(panic) => {
                let text = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "callback panicked".to_string());
                Err(ClaudeSdkError::ControlProtocol(format!(
                    "callback panicked: {text}"
                )))
            }
        };

        let response = match outcome {
            Ok(response_data) => json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": response_data,
                },
            }),
            Err(e) => json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": e.to_string(),
                },
            }),
        };
        let _ = self.transport.write(&format!("{}\n", response)).await;
    }

    async fn dispatch_control_request(
        self: &Arc<Self>,
        subtype: &str,
        request_data: &Value,
    ) -> Result<Value> {
        match subtype {
            "can_use_tool" => {
                let Some(can_use_tool) = &self.can_use_tool else {
                    return Err(ClaudeSdkError::ControlProtocol(
                        "canUseTool callback is not provided".to_string(),
                    ));
                };
                let original_input = request_data
                    .get("input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let suggestions: Vec<PermissionUpdate> = request_data
                    .get("permission_suggestions")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|s| serde_json::from_value(s.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let get = |key: &str| {
                    request_data
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                };
                let context = ToolPermissionContext {
                    suggestions,
                    tool_use_id: get("tool_use_id"),
                    agent_id: get("agent_id"),
                    blocked_path: get("blocked_path"),
                    decision_reason: get("decision_reason"),
                    title: get("title"),
                    display_name: get("display_name"),
                    description: get("description"),
                };
                let tool_name = request_data
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                let response = can_use_tool(tool_name, original_input.clone(), context).await?;

                match response {
                    PermissionResult::Allow(allow) => {
                        let mut response_data = json!({
                            "behavior": "allow",
                            "updatedInput": allow.updated_input.unwrap_or(original_input),
                        });
                        if let Some(updated_permissions) = allow.updated_permissions {
                            response_data["updatedPermissions"] =
                                serde_json::to_value(updated_permissions).unwrap_or(Value::Null);
                        }
                        Ok(response_data)
                    }
                    PermissionResult::Deny(deny) => {
                        let mut response_data = json!({
                            "behavior": "deny",
                            "message": deny.message,
                        });
                        if deny.interrupt {
                            response_data["interrupt"] = json!(true);
                        }
                        Ok(response_data)
                    }
                }
            }

            "hook_callback" => {
                let callback_id = request_data
                    .get("callback_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let callback = self
                    .state
                    .hook_callbacks
                    .lock()
                    .await
                    .get(&callback_id)
                    .cloned();
                let Some(callback) = callback else {
                    return Err(ClaudeSdkError::ControlProtocol(format!(
                        "No hook callback found for ID: {callback_id}"
                    )));
                };
                let input = HookInput::from_value(
                    request_data.get("input").cloned().unwrap_or(Value::Null),
                );
                let tool_use_id = request_data
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let hook_output = callback(input, tool_use_id, HookContext::default()).await?;
                // HookJsonOutput serializes straight to the CLI's field names
                // (`async`, `continue`, ...).
                serde_json::to_value(hook_output).map_err(|e| {
                    ClaudeSdkError::ControlProtocol(format!("Failed to serialize hook output: {e}"))
                })
            }

            "mcp_message" => {
                let server_name = request_data
                    .get("server_name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mcp_message = request_data.get("message").cloned();
                let (server_name, mcp_message) = match (server_name, mcp_message) {
                    (name, Some(message)) if !name.is_empty() && message.is_object() => {
                        (name.to_string(), message)
                    }
                    _ => {
                        return Err(ClaudeSdkError::ControlProtocol(
                            "Missing server_name or message for MCP request".to_string(),
                        ));
                    }
                };
                let mut mcp_response = self.handle_sdk_mcp_request(&server_name, mcp_message).await;
                if mcp_response.is_none() {
                    // JSON-RPC notifications get no reply, but the control
                    // request that carried one still expects an ack.
                    mcp_response = Some(json!({"jsonrpc": "2.0", "result": {}}));
                }
                Ok(json!({"mcp_response": mcp_response}))
            }

            other => Err(ClaudeSdkError::ControlProtocol(format!(
                "Unsupported control request subtype: {other}"
            ))),
        }
    }

    /// Route a JSON-RPC message from the CLI to the named SDK MCP server.
    ///
    /// Returns the JSON-RPC response for requests, or `None` when the message
    /// was a notification or response and there is nothing to send back. A
    /// message that cannot be delivered at all (unknown server, handler
    /// failure) is answered with a JSON-RPC error so the CLI's MCP client can
    /// fail that one request.
    async fn handle_sdk_mcp_request(&self, server_name: &str, message: Value) -> Option<Value> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let Some(server) = self.sdk_mcp_servers.get(server_name) else {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Server '{server_name}' not found")},
            }));
        };
        match server.handle_message(message).await {
            Ok(response) => response,
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": e.to_string()},
            })),
        }
    }

    async fn send_control_request(&self, request: Value, timeout: Duration) -> Result<Value> {
        if !self.is_streaming_mode {
            return Err(ClaudeSdkError::ControlProtocol(
                "Control requests require streaming mode".to_string(),
            ));
        }

        let counter = self.state.request_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let request_id = format!(
            "req_{counter}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );

        let (tx, rx) = oneshot::channel();
        self.state
            .pending_control
            .lock()
            .await
            .insert(request_id.clone(), tx);

        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let control_request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });

        if let Err(e) = self
            .transport
            .write(&format!("{}\n", control_request))
            .await
        {
            self.state.pending_control.lock().await.remove(&request_id);
            return Err(e);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => {
                let response_data = result.get("response").cloned().unwrap_or(json!({}));
                Ok(if response_data.is_object() {
                    response_data
                } else {
                    json!({})
                })
            }
            Ok(Ok(Err(error_text))) => Err(ClaudeSdkError::ControlProtocol(error_text)),
            Ok(Err(_)) => Err(ClaudeSdkError::ControlProtocol(format!(
                "Control request failed: {subtype}"
            ))),
            Err(_) => {
                self.state.pending_control.lock().await.remove(&request_id);
                Err(ClaudeSdkError::ControlProtocol(format!(
                    "Control request timeout: {subtype}"
                )))
            }
        }
    }

    async fn send_simple_control_request(&self, request: Value) -> Result<Value> {
        self.send_control_request(request, CONTROL_REQUEST_TIMEOUT)
            .await
    }

    /// Get current MCP server connection status.
    pub(crate) async fn get_mcp_status(&self) -> Result<Value> {
        self.send_simple_control_request(json!({"subtype": "mcp_status"}))
            .await
    }

    /// Get a breakdown of current context window usage by category.
    pub(crate) async fn get_context_usage(&self) -> Result<Value> {
        self.send_simple_control_request(json!({"subtype": "get_context_usage"}))
            .await
    }

    /// Send an interrupt control request.
    pub(crate) async fn interrupt(&self) -> Result<()> {
        self.send_simple_control_request(json!({"subtype": "interrupt"}))
            .await
            .map(|_| ())
    }

    /// Change the permission mode.
    pub(crate) async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "set_permission_mode",
            "mode": mode.as_str(),
        }))
        .await
        .map(|_| ())
    }

    /// Change the AI model.
    pub(crate) async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "set_model",
            "model": model,
        }))
        .await
        .map(|_| ())
    }

    /// Rewind tracked files to their state at a specific user message.
    pub(crate) async fn rewind_files(&self, user_message_id: &str) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "rewind_files",
            "user_message_id": user_message_id,
        }))
        .await
        .map(|_| ())
    }

    /// Reconnect a disconnected or failed MCP server.
    pub(crate) async fn reconnect_mcp_server(&self, server_name: &str) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "mcp_reconnect",
            "serverName": server_name,
        }))
        .await
        .map(|_| ())
    }

    /// Enable or disable an MCP server.
    pub(crate) async fn toggle_mcp_server(&self, server_name: &str, enabled: bool) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "mcp_toggle",
            "serverName": server_name,
            "enabled": enabled,
        }))
        .await
        .map(|_| ())
    }

    /// Stop a running task.
    pub(crate) async fn stop_task(&self, task_id: &str) -> Result<()> {
        self.send_simple_control_request(json!({
            "subtype": "stop_task",
            "task_id": task_id,
        }))
        .await
        .map(|_| ())
    }

    /// Track in-flight tasks from `system` task lifecycle frames.
    ///
    /// `task_started` marks a task in flight; `task_notification` or a
    /// `task_updated` patch with a terminal status clears it. Terminal
    /// completion can arrive as either frame (not every terminal task emits a
    /// notification), so both are handled. Only delegated agent work is
    /// tracked ([`DEFERRING_TASK_TYPES`]): a background shell may never reach
    /// a terminal status, and the CLI in stream-json mode only exits on stdin
    /// EOF, so tracking one would withhold the close forever rather than
    /// briefly. `background_tasks_changed` is deliberately not consumed in
    /// either direction — its payload is the live *background* set, which
    /// both omits tracked foreground work and includes untracked task types.
    async fn track_task_lifecycle(&self, message: &Value) {
        let subtype = message.get("subtype").and_then(Value::as_str).unwrap_or("");
        let Some(task_id) = message
            .get("task_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        match subtype {
            "task_started" => {
                let task_type = message.get("task_type").and_then(Value::as_str);
                if task_type.is_some_and(|t| DEFERRING_TASK_TYPES.contains(&t)) {
                    self.state
                        .inflight_tasks
                        .lock()
                        .await
                        .insert(task_id.to_string());
                }
            }
            "task_notification" => {
                self.state.inflight_tasks.lock().await.remove(task_id);
            }
            "task_updated" => {
                let status = message
                    .get("patch")
                    .and_then(|p| p.get("status"))
                    .and_then(Value::as_str);
                if status.is_some_and(is_terminal_task_status) {
                    self.state.inflight_tasks.lock().await.remove(task_id);
                }
            }
            _ => {}
        }
    }

    /// Whether the CLI may still send control requests that need a reply.
    ///
    /// SDK MCP servers, hooks, and the `can_use_tool` permission callback are
    /// all served over the control protocol: the CLI writes a
    /// `control_request` to stdout and blocks until the SDK writes the
    /// matching `control_response` to stdin. Closing stdin while any of these
    /// are configured makes every later request fail CLI-side with "Stream
    /// closed". Mirrors the TypeScript SDK's `hasBidirectionalNeeds`.
    fn has_bidirectional_needs(&self) -> bool {
        !self.sdk_mcp_servers.is_empty()
            || self.hooks.as_ref().is_some_and(|h| !h.is_empty())
            || self.can_use_tool.is_some()
    }

    /// Wait for the closing result (if needed) then close stdin.
    ///
    /// If SDK MCP servers, hooks, or a `can_use_tool` callback require
    /// bidirectional communication, keeps stdin open until the first result
    /// frame that arrives with no tasks in flight. A result frame ends one
    /// turn, not necessarily the run: background tasks keep running past it
    /// and still need stdin for control responses. The control protocol
    /// requires stdin to remain open for the entire conversation, so no
    /// timeout is applied. The event is guaranteed to fire: either when a
    /// result message arrives with no in-flight tasks, or in the read loop's
    /// teardown if the process exits early.
    ///
    /// Known limitation (same as the Python SDK): the event is one-shot and
    /// is not aware of prompt messages still queued CLI-side, so a stream
    /// prompt that yields several user messages (several turns) releases the
    /// hold at the first turn boundary with no tracked tasks; control
    /// requests from later turns can then find stdin closed. Single-message
    /// and string prompts — the common one-shot shapes — are fully covered.
    pub(crate) async fn wait_for_result_and_end_input(&self) {
        if self.has_bidirectional_needs() {
            tracing::debug!(
                target: "clawde",
                "Waiting for a run-ending result before closing stdin \
                 (sdk_mcp_servers={}, has_hooks={}, has_can_use_tool={})",
                self.sdk_mcp_servers.len(),
                self.hooks.is_some(),
                self.can_use_tool.is_some(),
            );
            let mut rx = self.state.first_result_tx.subscribe();
            // wait_for returns immediately if the value is already true.
            let _ = rx.wait_for(|fired| *fired).await;
        }
        let _ = self.transport.end_input().await;
    }

    /// Stream input messages to the transport.
    ///
    /// If SDK MCP servers, hooks, or a `can_use_tool` callback are present,
    /// waits for a run-ending result before closing stdin to allow
    /// bidirectional control protocol communication.
    pub(crate) async fn stream_input(self: Arc<Self>, mut stream: BoxStream<'static, Value>) {
        let mut written: usize = 0;
        while let Some(message) = stream.next().await {
            if self.state.closed.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = self.transport.write(&format!("{message}\n")).await {
                // The write failed. Don't leave stdin open — the CLI would
                // wait for input forever and the consumer's stream would
                // never finish — fall through and close it like a normal end
                // of input.
                tracing::error!(
                    target: "clawde",
                    "Prompt stream failed; closing stdin: {e}"
                );
                break;
            }
            written += 1;
        }
        if written > 0 {
            self.wait_for_result_and_end_input().await;
        } else {
            // Nothing was sent, so no result will arrive to release the
            // hold; close immediately (mirrors the TypeScript SDK's
            // messageCount guard).
            let _ = self.transport.end_input().await;
        }
    }

    /// Take the consumer-facing message receiver. May be called once.
    pub(crate) async fn take_message_receiver(&self) -> Option<mpsc::Receiver<QueryMessage>> {
        self.message_rx.lock().await.take()
    }

    /// Close the query and transport.
    ///
    /// Final-flushes the mirror batcher, aborts child tasks and in-flight
    /// control handlers, stops the read task, and closes the transport. Safe
    /// to call more than once.
    pub(crate) async fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        // Final-flush mirror entries before tearing down so an early close
        // doesn't drop the current turn when the process exits immediately.
        if let Some(batcher) = self.state.mirror_batcher.lock().await.as_ref() {
            batcher.close().await;
        }
        for task in self.state.child_tasks.lock().await.drain(..) {
            task.abort();
        }
        for (_, task) in self.state.inflight_requests.lock().await.drain() {
            task.abort();
        }
        if let Some(read_task) = self.state.read_task.lock().await.take() {
            read_task.abort();
            let _ = read_task.await;
        }
        // Give each in-process MCP server a chance to release its resources,
        // bounded per server like the Python bridge's shutdown grace period.
        // Isolated: user-supplied close() code that panics must not unwind
        // out of this shutdown path — transport.close() below still has to
        // run or the subprocess leaks.
        for (name, server) in &self.sdk_mcp_servers {
            let close = std::panic::AssertUnwindSafe(tokio::time::timeout(
                SDK_MCP_SHUTDOWN_GRACE,
                server.close(),
            ));
            match futures::FutureExt::catch_unwind(close).await {
                Ok(Ok(())) => {}
                Ok(Err(_timed_out)) => {
                    tracing::warn!(
                        target: "clawde",
                        "SDK MCP server {name:?} did not stop within {}s of being closed; \
                         no longer waiting for it",
                        SDK_MCP_SHUTDOWN_GRACE.as_secs(),
                    );
                }
                Err(_panicked) => {
                    tracing::warn!(
                        target: "clawde",
                        "SDK MCP server {name:?} panicked in close(); continuing shutdown",
                    );
                }
            }
        }
        let _ = self.message_tx.try_send(QueryMessage::End);
        let _ = self.transport.close().await;
    }
}
