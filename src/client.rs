//! [`ClaudeSdkClient`] for bidirectional, interactive conversations with
//! Claude Code.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use crate::errors::{ClaudeSdkError, Result};
use crate::internal::query::{Query, QueryMessage};
use crate::internal::session_resume::MaterializedResume;
use crate::message_parser::parse_message;
use crate::query::{connect_internal, string_prompt_message, Connected, QueryPrompt};
use crate::transport::Transport;
use crate::types::{
    ClaudeAgentOptions, ContextUsageResponse, McpStatusResponse, Message, PermissionMode,
};

struct ClientConnection {
    query: Arc<Query>,
    transport: Arc<dyn Transport>,
    receiver: Arc<Mutex<mpsc::Receiver<QueryMessage>>>,
    materialized: Option<Arc<MaterializedResume>>,
    // Captured at connect (inside the caller's runtime) so Drop can spawn
    // cleanup even from a non-runtime thread.
    runtime: tokio::runtime::Handle,
}

/// Client for bidirectional, interactive conversations with Claude Code.
///
/// This client provides full control over the conversation flow with support
/// for streaming, interrupts, and dynamic message sending. For simple
/// one-shot queries, consider using [`crate::query`] instead.
///
/// Key features:
/// - **Bidirectional**: send and receive messages at any time.
/// - **Stateful**: maintains conversation context across messages.
/// - **Interactive**: send follow-ups based on responses.
/// - **Control flow**: supports interrupts and session management.
///
/// # Example
///
/// ```no_run
/// use clawde::{ClaudeAgentOptions, ClaudeSdkClient, Message};
/// use futures::StreamExt;
///
/// # async fn example() -> clawde::Result<()> {
/// let mut client = ClaudeSdkClient::new(ClaudeAgentOptions::default());
/// client.connect(None).await?;
/// client.query("What's the capital of France?", None).await?;
/// let mut responses = client.receive_response();
/// while let Some(message) = responses.next().await {
///     println!("{:?}", message?);
/// }
/// drop(responses);
/// client.disconnect().await?;
/// # Ok(())
/// # }
/// ```
pub struct ClaudeSdkClient {
    /// The options this client was created with.
    pub options: ClaudeAgentOptions,
    custom_transport: Option<Arc<dyn Transport>>,
    connection: Option<ClientConnection>,
}

impl ClaudeSdkClient {
    /// Create a client with the given options.
    pub fn new(options: ClaudeAgentOptions) -> Self {
        Self {
            options,
            custom_transport: None,
            connection: None,
        }
    }

    /// Create a client that uses a custom [`Transport`] instead of the
    /// default subprocess transport.
    pub fn with_transport(options: ClaudeAgentOptions, transport: Arc<dyn Transport>) -> Self {
        Self {
            options,
            custom_transport: Some(transport),
            connection: None,
        }
    }

    /// Connect to Claude with an optional prompt or message stream.
    ///
    /// With `None`, connects with an empty stream for fully interactive use:
    /// send turns with [`ClaudeSdkClient::query`].
    pub async fn connect(&mut self, prompt: Option<QueryPrompt>) -> Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let connected = connect_internal(self.options.clone(), self.custom_transport.clone()).await;
        let Connected {
            query,
            transport,
            materialized,
        } = match connected {
            Ok(connected) => connected,
            Err(e) => return Err(e),
        };

        // If we have an initial prompt, send it.
        match prompt {
            Some(QueryPrompt::Text(text)) => {
                let message = string_prompt_message(&text, "default");
                if let Err(e) = transport.write(&format!("{message}\n")).await {
                    query.close().await;
                    if let Some(materialized) = &materialized {
                        materialized.cleanup().await;
                    }
                    return Err(e);
                }
            }
            Some(QueryPrompt::Stream(stream)) => {
                let streamer = query.clone();
                query
                    .spawn_task(async move { streamer.stream_input(stream).await })
                    .await;
            }
            // No prompt: keep the connection open for interactive use.
            None => {}
        }

        let receiver = query
            .take_message_receiver()
            .await
            .expect("message receiver already taken");
        self.connection = Some(ClientConnection {
            query,
            transport,
            receiver: Arc::new(Mutex::new(receiver)),
            materialized,
            runtime: tokio::runtime::Handle::current(),
        });
        Ok(())
    }

    fn connection(&self) -> Result<&ClientConnection> {
        self.connection
            .as_ref()
            .ok_or_else(|| ClaudeSdkError::cli_connection("Not connected. Call connect() first."))
    }

    /// Receive all messages from Claude as a stream.
    ///
    /// The stream ends when the connection closes. Only one receive stream
    /// should be consumed at a time; messages are delivered to whichever
    /// stream polls first.
    pub fn receive_messages(&self) -> BoxStream<'static, Result<Message>> {
        let Ok(connection) = self.connection() else {
            return futures::stream::once(async {
                Err(ClaudeSdkError::cli_connection(
                    "Not connected. Call connect() first.",
                ))
            })
            .boxed();
        };
        let receiver = connection.receiver.clone();
        futures::stream::unfold((receiver, false), |(receiver, done)| async move {
            if done {
                return None;
            }
            loop {
                let next = receiver.lock().await.recv().await;
                match next {
                    None | Some(QueryMessage::End) => return None,
                    Some(QueryMessage::Error(e)) => return Some((Err(e), (receiver, true))),
                    Some(QueryMessage::Data(data)) => match parse_message(&data) {
                        // Skip unknown message types.
                        Ok(None) => continue,
                        Ok(Some(message)) => return Some((Ok(message), (receiver, false))),
                        Err(e) => return Some((Err(e), (receiver, true))),
                    },
                }
            }
        })
        .boxed()
    }

    /// Receive messages from Claude until and including a
    /// [`Message::Result`].
    ///
    /// Yields each message as it's received and terminates immediately after
    /// yielding a result message (which IS included). A convenience over
    /// [`ClaudeSdkClient::receive_messages`] for single-response workflows.
    pub fn receive_response(&self) -> BoxStream<'static, Result<Message>> {
        // Terminates immediately after yielding a result message, without
        // polling the underlying stream again — the session may produce no
        // further messages until the next prompt is sent.
        futures::stream::unfold(
            (self.receive_messages(), false),
            |(mut inner, done)| async move {
                if done {
                    return None;
                }
                let item = inner.next().await?;
                let is_result = matches!(&item, Ok(Message::Result(_)));
                Some((item, (inner, is_result)))
            },
        )
        .boxed()
    }

    /// Send a new request in streaming mode.
    ///
    /// `session_id` defaults to `"default"`. For a stream prompt, each
    /// message object gets the session id filled in when absent.
    pub async fn query(
        &self,
        prompt: impl Into<QueryPrompt>,
        session_id: Option<&str>,
    ) -> Result<()> {
        let connection = self.connection()?;
        let session_id = session_id.unwrap_or("default");
        match prompt.into() {
            QueryPrompt::Text(text) => {
                let message = string_prompt_message(&text, session_id);
                connection.transport.write(&format!("{message}\n")).await
            }
            QueryPrompt::Stream(mut stream) => {
                while let Some(mut message) = stream.next().await {
                    // Ensure session_id is set on each message.
                    if let Value::Object(obj) = &mut message {
                        obj.entry("session_id")
                            .or_insert_with(|| Value::String(session_id.to_string()));
                    }
                    connection.transport.write(&format!("{message}\n")).await?;
                }
                Ok(())
            }
        }
    }

    /// Send an interrupt signal (only works with streaming mode).
    pub async fn interrupt(&self) -> Result<()> {
        self.connection()?.query.interrupt().await
    }

    /// Change permission mode during the conversation.
    ///
    /// ```no_run
    /// # use clawde::{ClaudeSdkClient, PermissionMode};
    /// # async fn example(client: &ClaudeSdkClient) -> clawde::Result<()> {
    /// // Review mode done, switch to auto-accept edits:
    /// client.set_permission_mode(PermissionMode::AcceptEdits).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.connection()?.query.set_permission_mode(mode).await
    }

    /// Change the AI model during the conversation. `None` selects the
    /// default model.
    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.connection()?.query.set_model(model).await
    }

    /// Rewind tracked files to their state at a specific user message.
    ///
    /// Requires [`ClaudeAgentOptions::enable_file_checkpointing`] to track
    /// file changes, and `extra_args: {"replay-user-messages": None}` to
    /// receive [`crate::UserMessage`]s with `uuid` in the response stream.
    /// `user_message_id` is the `uuid` field from a user message received
    /// during the conversation.
    pub async fn rewind_files(&self, user_message_id: &str) -> Result<()> {
        self.connection()?.query.rewind_files(user_message_id).await
    }

    /// Reconnect a disconnected or failed MCP server.
    ///
    /// Use this to retry connecting to an MCP server that failed to connect
    /// or was disconnected. Fails if the reconnection fails.
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> Result<()> {
        self.connection()?
            .query
            .reconnect_mcp_server(server_name)
            .await
    }

    /// Enable or disable an MCP server.
    ///
    /// Disabling a server disconnects it and removes its tools from the
    /// available tool set. Enabling a server reconnects it and makes its
    /// tools available again.
    pub async fn toggle_mcp_server(&self, server_name: &str, enabled: bool) -> Result<()> {
        self.connection()?
            .query
            .toggle_mcp_server(server_name, enabled)
            .await
    }

    /// Stop a running task.
    ///
    /// After this resolves, a `task_notification` system message with status
    /// `stopped` will be emitted by the CLI in the message stream.
    /// `task_id` comes from [`crate::TaskNotificationMessage`] /
    /// [`crate::TaskStartedMessage`] events.
    pub async fn stop_task(&self, task_id: &str) -> Result<()> {
        self.connection()?.query.stop_task(task_id).await
    }

    /// Get current MCP server connection status.
    ///
    /// Queries the Claude Code CLI for the live connection status of all
    /// configured MCP servers.
    pub async fn get_mcp_status(&self) -> Result<McpStatusResponse> {
        let raw = self.connection()?.query.get_mcp_status().await?;
        serde_json::from_value(raw).map_err(|e| {
            ClaudeSdkError::ControlProtocol(format!("Invalid mcp_status response: {e}"))
        })
    }

    /// Get a breakdown of current context window usage by category.
    ///
    /// Returns the same data shown by the `/context` command in the CLI,
    /// including token counts per category, total usage, and detailed
    /// breakdowns of MCP tools, memory files, and agents.
    pub async fn get_context_usage(&self) -> Result<ContextUsageResponse> {
        let raw = self.connection()?.query.get_context_usage().await?;
        serde_json::from_value(raw).map_err(|e| {
            ClaudeSdkError::ControlProtocol(format!("Invalid context usage response: {e}"))
        })
    }

    /// Get server initialization info including available commands and
    /// output styles.
    ///
    /// Returns the initialization response obtained during connect, or
    /// `None` when unavailable.
    pub async fn get_server_info(&self) -> Result<Option<Value>> {
        Ok(self.connection()?.query.initialization_result().await)
    }

    /// Disconnect from Claude.
    ///
    /// Closes the subprocess (or custom transport) and cleans up any
    /// materialized resume directory. Safe to call when not connected.
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(connection) = self.connection.take() {
            connection.query.close().await;
            if let Some(materialized) = &connection.materialized {
                materialized.cleanup().await;
            }
        }
        Ok(())
    }
}

impl Drop for ClaudeSdkClient {
    fn drop(&mut self) {
        // Best-effort cleanup for clients dropped without disconnect(): the
        // read task holds the query alive, so spawn a close to terminate the
        // subprocess rather than leaking it until process exit. Uses the
        // handle captured at connect so this works from non-runtime threads.
        if let Some(connection) = self.connection.take() {
            crate::query::spawn_cleanup_on(
                &connection.runtime,
                Some(connection.query),
                connection.materialized,
            );
        }
    }
}
