//! Session store and session listing types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::errors::{ClaudeSdkError, Result};

/// One JSONL transcript line as observed by a [`SessionStore`] adapter.
///
/// The concrete shape is the CLI's on-disk transcript format (a large
/// discriminated union). That union is internal, so entries are treated as
/// pass-through JSON objects; round-tripping through serialization is the
/// only required invariant. Most entries carry a `type` and a stable `uuid`.
pub type SessionStoreEntry = Map<String, Value>;

/// Identifies a session transcript or subagent transcript in a store.
///
/// Main transcripts have no `subpath`; subagent transcripts include a
/// `subpath` like `"subagents/agent-{id}"` that mirrors the on-disk directory
/// structure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Caller-defined scope. Default: sanitized cwd. Multi-tenant deployments
    /// should set this to a tenant ID or project name. Paths longer than 200
    /// characters are truncated and suffixed with a portable hash so the same
    /// path yields the same key across runtimes.
    pub project_key: String,
    /// Session id (UUID).
    pub session_id: String,
    /// Omit for the main transcript; set for subagent files. Empty string is
    /// invalid — omit the field for the main transcript. Opaque to the
    /// adapter — just use it as a storage key suffix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

impl SessionKey {
    /// Key for a main transcript.
    pub fn new(project_key: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            project_key: project_key.into(),
            session_id: session_id.into(),
            subpath: None,
        }
    }

    /// Key for a subagent transcript.
    pub fn with_subpath(
        project_key: impl Into<String>,
        session_id: impl Into<String>,
        subpath: impl Into<String>,
    ) -> Self {
        Self {
            project_key: project_key.into(),
            session_id: session_id.into(),
            subpath: Some(subpath.into()),
        }
    }
}

/// Entry returned by [`SessionStore::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStoreListEntry {
    /// Session id.
    pub session_id: String,
    /// Last-modified time in Unix epoch milliseconds. Adapters without
    /// native modification time (e.g. Redis) must maintain their own index.
    pub mtime: i64,
}

/// Incrementally-maintained session summary.
///
/// Stores obtain this from [`crate::fold_session_summary`] inside
/// [`SessionStore::append`] and persist it verbatim; they return the full set
/// from [`SessionStore::list_session_summaries`]. The `data` field is opaque
/// SDK-owned state — stores MUST NOT interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummaryEntry {
    /// Session id.
    pub session_id: String,
    /// Storage write time of the sidecar, in Unix epoch milliseconds. Must
    /// use the same clock source as the `mtime` returned by
    /// [`SessionStore::list_sessions`] for this session. Do NOT derive this
    /// from entry ISO timestamps. [`crate::fold_session_summary`] preserves
    /// whatever `mtime` the caller passes in via `prev` and does not set it
    /// itself; stamp it after persisting.
    pub mtime: i64,
    /// Opaque SDK-owned summary state. Persist verbatim; do not interpret.
    pub data: Map<String, Value>,
}

/// Key argument to [`SessionStore::list_subkeys`] (no `subpath`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListSubkeysKey {
    /// Project scope.
    pub project_key: String,
    /// Session id.
    pub session_id: String,
}

/// Controls when transcript-mirror entries are flushed to a [`SessionStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStoreFlushMode {
    /// Buffer entries and flush once per turn (on the `result` message) or
    /// when the pending buffer exceeds 500 entries / 1 MiB. Keeps adapter
    /// latency off the streaming hot path.
    #[default]
    Batched,
    /// Trigger a background flush after every `transcript_mirror` frame so
    /// [`SessionStore::append`] sees entries in near real time. Appends are
    /// still serialized in enqueue order; a slow adapter will not stall the
    /// read loop but will see frames coalesced while it is busy.
    Eager,
}

/// Optional [`SessionStore`] methods, used with
/// [`SessionStore::implements`] to probe adapter capabilities before calling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStoreMethod {
    /// [`SessionStore::list_sessions`].
    ListSessions,
    /// [`SessionStore::list_session_summaries`].
    ListSessionSummaries,
    /// [`SessionStore::delete`].
    Delete,
    /// [`SessionStore::list_subkeys`].
    ListSubkeys,
}

/// Adapter for mirroring session transcripts to external storage.
///
/// The subprocess still writes to local disk (set `CLAUDE_CONFIG_DIR=/tmp`
/// for an ephemeral local copy); the adapter receives a secondary copy.
///
/// The SDK never deletes from your store unless you call
/// [`crate::delete_session_via_store`] with [`SessionStore::delete`]
/// implemented. Retention is the adapter's responsibility.
///
/// Only [`SessionStore::append`] and [`SessionStore::load`] are required. The
/// remaining methods are optional: their default implementations return
/// [`ClaudeSdkError::StoreUnimplemented`], and call sites probe
/// [`SessionStore::implements`] before invoking them — override it alongside
/// any optional method you implement.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Mirror a batch of transcript entries.
    ///
    /// Called AFTER the subprocess's local write succeeds — durability is
    /// already guaranteed locally. Batches arrive at ~100ms cadence during
    /// active turns.
    ///
    /// Within a single process, persist entries in append-call order; across
    /// concurrent processes, order is by storage commit time, not call time.
    ///
    /// Most entries carry a stable `uuid` that adapters should treat as an
    /// idempotency key (upsert / ignore-duplicate). Entries without a `uuid`
    /// (e.g. titles, tags, mode markers) should be appended without dedup.
    /// Errors are logged and the subprocess continues unaffected — failed
    /// batches are retried (3 attempts total) with short backoff before being
    /// dropped and surfaced as a [`crate::MirrorErrorMessage`]; timeouts are
    /// not retried since the in-flight call may still land.
    async fn append(&self, key: &SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()>;

    /// Load a full session for resume.
    ///
    /// Called once, in the SDK parent, before subprocess spawn. The result is
    /// materialized to a temporary JSONL file; the subprocess resumes from
    /// that file using its existing resume code.
    ///
    /// Return `None` for a key that was never written; adapters that cannot
    /// distinguish "never written" from "emptied" may return `None` for both.
    /// Returned entries must be deep-equal to what was appended — byte-equal
    /// serialization is NOT required; the SDK never hashes or byte-compares
    /// entries.
    async fn load(&self, key: &SessionKey) -> Result<Option<Vec<SessionStoreEntry>>>;

    /// List sessions for a `project_key`. Returns IDs + modification times.
    ///
    /// `mtime` is Unix epoch milliseconds. Result order is unspecified — the
    /// SDK sorts by `mtime` descending.
    ///
    /// Optional — if unimplemented, [`crate::list_sessions_from_store`]
    /// without summaries fails.
    async fn list_sessions(&self, project_key: &str) -> Result<Vec<SessionStoreListEntry>> {
        let _ = project_key;
        Err(ClaudeSdkError::StoreUnimplemented {
            method: "list_sessions",
        })
    }

    /// Return incrementally-maintained summaries for all sessions in one
    /// call.
    ///
    /// Stores should maintain these via [`crate::fold_session_summary`]
    /// inside [`SessionStore::append`]. Skip the fold for keys with a
    /// `subpath` — subagent transcripts must not contribute to the main
    /// session's summary.
    ///
    /// Optional — if unimplemented, [`crate::list_sessions_from_store`]
    /// falls back to [`SessionStore::list_sessions`] + per-session
    /// [`SessionStore::load`].
    ///
    /// Stores that maintain summaries inside `append()` MUST serialize
    /// sidecar writes if `append()` calls can race for the same session.
    /// [`crate::fold_session_summary`] is pure; concurrency control is the
    /// store's responsibility.
    async fn list_session_summaries(&self, project_key: &str) -> Result<Vec<SessionSummaryEntry>> {
        let _ = project_key;
        Err(ClaudeSdkError::StoreUnimplemented {
            method: "list_session_summaries",
        })
    }

    /// Delete a session.
    ///
    /// Deleting a main-transcript key (no `subpath`) must cascade to all
    /// subkeys under that session so subagent transcripts aren't orphaned. A
    /// targeted delete with an explicit `subpath` removes only that one
    /// entry.
    ///
    /// Optional — if unimplemented, deletion is a no-op (appropriate for
    /// WORM/append-only backends like object storage).
    async fn delete(&self, key: &SessionKey) -> Result<()> {
        let _ = key;
        Err(ClaudeSdkError::StoreUnimplemented { method: "delete" })
    }

    /// List all subpath keys under a session (e.g. subagent transcripts).
    ///
    /// Used during resume to discover and materialize all subagent data.
    ///
    /// Optional — if unimplemented, resume only materializes the main
    /// transcript.
    async fn list_subkeys(&self, key: &SessionListSubkeysKey) -> Result<Vec<String>> {
        let _ = key;
        Err(ClaudeSdkError::StoreUnimplemented {
            method: "list_subkeys",
        })
    }

    /// Whether this store implements the given optional method. The SDK
    /// probes this before calling optional methods (mirroring the Python
    /// SDK's runtime protocol probing); override it alongside any optional
    /// method you implement.
    fn implements(&self, method: SessionStoreMethod) -> bool {
        let _ = method;
        false
    }
}

/// Session metadata returned by [`crate::list_sessions`].
///
/// Contains only data extractable from stat + head/tail reads — no full
/// JSONL parsing required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkSessionInfo {
    /// Unique session identifier (UUID).
    pub session_id: String,
    /// Display title for the session — custom title, auto-generated summary,
    /// or first prompt.
    pub summary: String,
    /// Last modified time in milliseconds since epoch.
    pub last_modified: i64,
    /// Session file size in bytes. Only populated for local JSONL storage;
    /// may be `None` for remote storage backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// Session title — user-set custom title or AI-generated title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    /// First meaningful user prompt in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Git branch at the end of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Working directory for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// User-set session tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Creation time in milliseconds since epoch, extracted from the first
    /// entry's ISO timestamp field. More reliable than filesystem birth time
    /// which is unsupported on some filesystems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

/// Whether a [`SessionMessage`] is a user or assistant message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMessageType {
    /// A user message.
    User,
    /// An assistant message.
    Assistant,
}

/// A user or assistant message from a session transcript.
///
/// Returned by [`crate::get_session_messages`] for reading historical
/// session data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Message type — user or assistant.
    #[serde(rename = "type")]
    pub message_type: SessionMessageType,
    /// Unique message identifier.
    pub uuid: String,
    /// ID of the session this message belongs to.
    pub session_id: String,
    /// Raw Anthropic API message object (role, content, etc.).
    pub message: Value,
    /// For messages returned by [`crate::get_subagent_messages`] /
    /// [`crate::get_subagent_messages_from_store`], the id of the Agent
    /// `tool_use` block in the parent session that spawned the subagent
    /// (recovered from the subagent's metadata; `None` if that metadata is
    /// unavailable). Always `None` for top-level session messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// For subagent messages, the agent id of the subagent that spawned this
    /// subagent, or `None` if it was spawned by the main session (or the
    /// metadata is unavailable). Always `None` for top-level session
    /// messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
}
