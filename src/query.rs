//! The [`query`] function for one-shot interactions with Claude Code.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::BoxStream;
use futures::Stream;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::errors::Result;
use crate::internal::query::{Query, QueryMessage, QueryOptions};
use crate::internal::session_resume::{
    apply_materialized_options, build_mirror_batcher, materialize_resume_session,
    validate_session_store_options, MaterializedResume,
};
use crate::message_parser::parse_message;
use crate::transport::{SubprocessCliTransport, Transport};
use crate::types::options::configure_can_use_tool;
use crate::types::{ClaudeAgentOptions, Message, SystemPrompt};

/// The prompt for a [`query`]: a string for single-shot queries, or a stream
/// of message objects for streaming input.
///
/// In streaming mode, each object should have the structure:
///
/// ```json
/// {
///     "type": "user",
///     "message": {"role": "user", "content": "..."},
///     "parent_tool_use_id": null,
///     "session_id": "..."
/// }
/// ```
pub enum QueryPrompt {
    /// A single prompt string.
    Text(String),
    /// A stream of message objects (streaming input mode).
    ///
    /// Known limitation with [`query`] (inherited from the Python SDK): when
    /// the stream yields several user messages (several turns) *and* hooks,
    /// SDK MCP servers, or a `can_use_tool` callback are configured, stdin
    /// closes at the first turn's result, so control requests from later
    /// turns can fail CLI-side. Use [`crate::ClaudeSdkClient`] for
    /// multi-turn conversations that need the control protocol.
    Stream(BoxStream<'static, Value>),
}

impl From<String> for QueryPrompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for QueryPrompt {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

pub(crate) struct Connected {
    pub query: Arc<Query>,
    pub transport: Arc<dyn Transport>,
    pub materialized: Option<Arc<MaterializedResume>>,
}

/// Shared connection flow for [`query`] and
/// [`crate::ClaudeSdkClient::connect`]: validates options, materializes a
/// store-backed resume, spawns (or adopts) the transport, and completes the
/// control-protocol initialize handshake.
pub(crate) async fn connect_internal(
    options: ClaudeAgentOptions,
    custom_transport: Option<Arc<dyn Transport>>,
) -> Result<Connected> {
    // Fail fast on invalid session_store option combinations before spawning
    // the subprocess.
    validate_session_store_options(&options)?;

    // resume/continue + session_store: load the session from the store into
    // a temp CLAUDE_CONFIG_DIR for the subprocess to resume from. Skipped
    // when a custom transport was supplied — the materialized options never
    // reach a pre-constructed transport, so loading the store and writing
    // .credentials.json to a temp dir would be wasted work.
    let materialized = if custom_transport.is_none() {
        materialize_resume_session(&options).await?.map(Arc::new)
    } else {
        None
    };

    let result = connect_inner(options, custom_transport, materialized.clone()).await;
    match result {
        Ok(connected) => Ok(connected),
        Err(e) => {
            // The temp dir holds a .credentials.json copy — remove it on
            // every failure path, including transport spawn failure.
            if let Some(materialized) = &materialized {
                materialized.cleanup().await;
            }
            Err(e)
        }
    }
}

async fn connect_inner(
    options: ClaudeAgentOptions,
    custom_transport: Option<Arc<dyn Transport>>,
    materialized: Option<Arc<MaterializedResume>>,
) -> Result<Connected> {
    // Validate and configure permission settings (matching the TypeScript
    // SDK logic).
    let mut configured = configure_can_use_tool(options)?;
    if let Some(materialized) = &materialized {
        configured = apply_materialized_options(configured, materialized);
    }

    // Extract what the Query needs before the transport takes ownership of
    // the options.
    let sdk_mcp_servers = configured.mcp_servers.sdk_servers();
    let can_use_tool = configured.can_use_tool.clone();
    let hooks = configured.hooks.clone();
    let session_store = configured.session_store.clone();
    let session_store_flush = configured.session_store_flush;
    let env = configured.env.clone();
    let forward_subagent_text = configured.forward_subagent_text;
    let skills = match &configured.skills {
        // 'all' and omitted are equivalent at the wire level (no filter).
        Some(crate::types::SkillsConfig::List(names)) => Some(names.clone()),
        _ => None,
    };
    // Extract exclude_dynamic_sections from the preset system prompt for the
    // initialize request (older CLIs ignore unknown initialize fields).
    let exclude_dynamic_sections = match &configured.system_prompt {
        Some(SystemPrompt::Preset {
            exclude_dynamic_sections,
            ..
        }) => *exclude_dynamic_sections,
        _ => None,
    };
    // Convert agents to the wire object for the initialize request.
    let agents = configured.agents.as_ref().map(|agents| {
        let map: serde_json::Map<String, Value> = agents
            .iter()
            .map(|(name, def)| {
                (
                    name.clone(),
                    serde_json::to_value(def).unwrap_or(Value::Null),
                )
            })
            .collect();
        Value::Object(map)
    });

    // Calculate the initialize timeout from CLAUDE_CODE_STREAM_CLOSE_TIMEOUT
    // (milliseconds) if set; minimum 60s since MCP servers may take time to
    // start.
    let initialize_timeout_ms = std::env::var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60_000);
    let initialize_timeout = Duration::from_millis(initialize_timeout_ms.max(60_000));

    let transport: Arc<dyn Transport> = match custom_transport {
        Some(transport) => transport,
        None => Arc::new(SubprocessCliTransport::new(configured)),
    };
    transport.connect().await?;

    let query = Query::new(
        transport.clone(),
        QueryOptions {
            can_use_tool,
            hooks,
            sdk_mcp_servers,
            initialize_timeout,
            agents,
            exclude_dynamic_sections,
            skills,
            forward_subagent_text,
        },
    );

    if let Some(store) = session_store {
        // The batcher's error callback feeds back into the query as a
        // synthesized `mirror_error` system message; a weak reference avoids
        // a reference cycle between query and batcher.
        let weak_query = Arc::downgrade(&query);
        let on_error: crate::internal::transcript_mirror_batcher::MirrorErrorCallback =
            Arc::new(move |key, error| {
                let weak_query = weak_query.clone();
                Box::pin(async move {
                    if let Some(query) = weak_query.upgrade() {
                        query.report_mirror_error(key.as_ref(), &error);
                    }
                })
            });
        let batcher = build_mirror_batcher(
            store,
            materialized.as_deref(),
            &env,
            on_error,
            session_store_flush,
        );
        query.set_transcript_mirror_batcher(batcher).await;
    }

    // Start reading messages and initialize.
    query.start().await;
    if let Err(e) = query.initialize().await {
        // If connect fails after the subprocess has spawned (e.g. at
        // initialize), close the subprocess/read task before the temp
        // CLAUDE_CONFIG_DIR it points at is removed by the caller.
        query.close().await;
        return Err(e);
    }

    Ok(Connected {
        query,
        transport,
        materialized,
    })
}

/// Build the stdin user message for a plain string prompt.
pub(crate) fn string_prompt_message(prompt: &str, session_id: &str) -> Value {
    json!({
        "type": "user",
        "session_id": session_id,
        "message": {"role": "user", "content": prompt},
        "parent_tool_use_id": null,
    })
}

/// Stream of [`Message`]s returned by [`query`].
///
/// Ends after the CLI's message stream closes. Dropping the stream (or
/// exhausting it) closes the underlying subprocess and cleans up any
/// materialized resume directory.
pub struct MessageStream {
    receiver: mpsc::Receiver<QueryMessage>,
    query: Option<Arc<Query>>,
    materialized: Option<Arc<MaterializedResume>>,
    // Captured at construction (inside the caller's runtime) so Drop can
    // spawn cleanup even from a non-runtime thread — Handle::try_current()
    // would fail there and leak the subprocess.
    runtime: tokio::runtime::Handle,
    done: bool,
}

impl MessageStream {
    pub(crate) fn new(
        receiver: mpsc::Receiver<QueryMessage>,
        query: Arc<Query>,
        materialized: Option<Arc<MaterializedResume>>,
    ) -> Self {
        Self {
            receiver,
            query: Some(query),
            materialized,
            runtime: tokio::runtime::Handle::current(),
            done: false,
        }
    }

    fn spawn_cleanup(&mut self) {
        let query = self.query.take();
        let materialized = self.materialized.take();
        if query.is_none() && materialized.is_none() {
            return;
        }
        spawn_cleanup_on(&self.runtime, query, materialized);
    }
}

/// Spawn subprocess/materialized-dir cleanup on the given runtime handle.
///
/// Guarded against a runtime that is mid-shutdown (where `spawn` panics):
/// in that case the runtime is tearing every task down anyway, which drops
/// the transport and lets `kill_on_drop` reap the child.
pub(crate) fn spawn_cleanup_on(
    runtime: &tokio::runtime::Handle,
    query: Option<Arc<Query>>,
    materialized: Option<Arc<MaterializedResume>>,
) {
    let runtime = runtime.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        runtime.spawn(async move {
            if let Some(query) = query {
                query.close().await;
            }
            if let Some(materialized) = materialized {
                materialized.cleanup().await;
            }
        });
    }));
}

impl Stream for MessageStream {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.done {
                return Poll::Ready(None);
            }
            match self.receiver.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) | Poll::Ready(Some(QueryMessage::End)) => {
                    self.done = true;
                    self.spawn_cleanup();
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(QueryMessage::Error(e))) => {
                    self.done = true;
                    self.spawn_cleanup();
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(Some(QueryMessage::Data(data))) => {
                    match parse_message(&data) {
                        // Skip unknown message types.
                        Ok(None) => continue,
                        Ok(Some(message)) => return Poll::Ready(Some(Ok(message))),
                        Err(e) => {
                            self.done = true;
                            self.spawn_cleanup();
                            return Poll::Ready(Some(Err(e)));
                        }
                    }
                }
            }
        }
    }
}

impl Drop for MessageStream {
    fn drop(&mut self) {
        self.spawn_cleanup();
    }
}

/// Query Claude Code for one-shot or unidirectional streaming interactions.
///
/// This function is ideal for simple, stateless queries where you don't need
/// bidirectional communication or conversation management. For interactive,
/// stateful conversations, use [`crate::ClaudeSdkClient`] instead.
///
/// Key differences from [`crate::ClaudeSdkClient`]:
/// - **Unidirectional**: send all messages upfront, receive all responses.
/// - **Stateless**: each query is independent, no conversation state.
/// - **Simple**: fire-and-forget style, no connection management.
/// - **No interrupts**: cannot interrupt or send follow-up messages.
///
/// # Examples
///
/// ```no_run
/// use clawde::{query, ClaudeAgentOptions};
/// use futures::StreamExt;
///
/// # async fn example() -> clawde::Result<()> {
/// let mut messages = query("What is the capital of France?", ClaudeAgentOptions::default()).await?;
/// while let Some(message) = messages.next().await {
///     println!("{:?}", message?);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn query(
    prompt: impl Into<QueryPrompt>,
    options: ClaudeAgentOptions,
) -> Result<MessageStream> {
    query_with_transport(prompt, options, None).await
}

/// [`query`] with a custom [`Transport`] implementation, used instead of the
/// default subprocess transport.
pub async fn query_with_transport(
    prompt: impl Into<QueryPrompt>,
    options: ClaudeAgentOptions,
    transport: Option<Arc<dyn Transport>>,
) -> Result<MessageStream> {
    let prompt = prompt.into();
    let connected = connect_internal(options, transport).await?;
    let Connected {
        query,
        transport,
        materialized,
    } = connected;

    match prompt {
        QueryPrompt::Text(text) => {
            // For string prompts, write the user message to stdin after
            // initialize (matching the TypeScript SDK behavior), then close
            // stdin once the run's closing result arrives.
            let message = string_prompt_message(&text, "");
            if let Err(e) = transport.write(&format!("{message}\n")).await {
                query.close().await;
                if let Some(materialized) = &materialized {
                    materialized.cleanup().await;
                }
                return Err(e);
            }
            let waiter = query.clone();
            query
                .spawn_task(async move { waiter.wait_for_result_and_end_input().await })
                .await;
        }
        QueryPrompt::Stream(stream) => {
            // Stream input in the background.
            let streamer = query.clone();
            query
                .spawn_task(async move { streamer.stream_input(stream).await })
                .await;
        }
    }

    let receiver = query
        .take_message_receiver()
        .await
        .expect("message receiver already taken");
    Ok(MessageStream::new(receiver, query, materialized))
}
