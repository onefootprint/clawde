//! Replay a local on-disk session transcript into a [`SessionStore`].
//!
//! This is the inverse of resume materialization — where the SDK reads a
//! store and writes a temp `~/.claude` tree, [`import_session_to_store`]
//! reads the local `~/.claude/projects/<dir>/<sessionId>.jsonl` (plus
//! subagent transcripts) and replays each line into `store.append()`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::errors::{ClaudeSdkError, Result};
use crate::internal::transcript_mirror_batcher::{MAX_PENDING_BYTES, MAX_PENDING_ENTRIES};
use crate::sessions::{is_valid_uuid, read_agent_metadata_sidecar, resolve_session_file_path};
use crate::types::{SessionKey, SessionStore, SessionStoreEntry};

/// Options for [`import_session_to_store`].
#[derive(Debug, Clone)]
pub struct ImportSessionOptions {
    /// Project directory path (same semantics as [`crate::list_sessions`]).
    /// When `None`, all project directories are searched for the session
    /// file.
    pub directory: Option<String>,
    /// If `true` (default), also import subagent transcripts under
    /// `<sessionId>/subagents/**` and their `.meta.json` sidecars.
    pub include_subagents: bool,
    /// Maximum entries per `store.append()` call. Default 500.
    pub batch_size: usize,
}

impl Default for ImportSessionOptions {
    fn default() -> Self {
        Self {
            directory: None,
            include_subagents: true,
            batch_size: MAX_PENDING_ENTRIES,
        }
    }
}

/// Replay a local session transcript into a [`SessionStore`].
///
/// Streams the on-disk JSONL line-by-line and calls `store.append(key,
/// batch)` every `batch_size` entries (or 1 MiB of line bytes, whichever
/// comes first). Useful for migrating existing local sessions to a remote
/// store, or for catching a store up after a [`crate::MirrorErrorMessage`]
/// indicated a live-mirror gap. Adapters should treat `entry["uuid"]` as an
/// idempotency key so re-import is duplicate-safe.
///
/// The destination `project_key` is the name of the on-disk project
/// directory the session file was found in — the same key the live mirror
/// would have produced for the same file — so an imported session is
/// indistinguishable from a live-mirrored one and resumable via
/// `ClaudeAgentOptions { session_store, resume, .. }` from the original cwd.
///
/// # Errors
///
/// [`ClaudeSdkError::InvalidConfig`] if `session_id` is not a valid UUID;
/// [`ClaudeSdkError::SessionNotFound`] if the session JSONL cannot be found
/// on disk.
pub async fn import_session_to_store(
    session_id: &str,
    store: &Arc<dyn SessionStore>,
    options: ImportSessionOptions,
) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }

    let Some(resolved) = resolve_session_file_path(session_id, options.directory.as_deref()) else {
        return Err(ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found"
        )));
    };

    // Key under the on-disk project directory name — matches the live
    // mirror's key derivation even when the resolver's search or worktree
    // fallback found the file somewhere other than `directory`.
    let project_key = resolved
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let batch_size = if options.batch_size == 0 {
        MAX_PENDING_ENTRIES
    } else {
        options.batch_size
    };

    let main_key = SessionKey::new(project_key.clone(), session_id);
    append_jsonl_file_in_batches(&resolved, &main_key, store, batch_size).await?;

    if !options.include_subagents {
        return Ok(());
    }

    // Subagent transcripts live at <projectDir>/<sessionId>/subagents/**.
    let session_dir = resolved.with_extension("");
    let subagents_dir = session_dir.join("subagents");
    for file_path in collect_jsonl_files(&subagents_dir) {
        // subpath is the path relative to session_dir, '/'-joined, sans
        // .jsonl — e.g. subagents/agent-abc or
        // subagents/workflows/run-1/agent-def. Matches the live mirror's key
        // derivation so list_subkeys() and get_subagent_messages_from_store()
        // round-trip.
        let Ok(rel) = file_path.strip_prefix(&session_dir) else {
            continue;
        };
        let mut rel_parts: Vec<String> = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
            .collect();
        if let Some(last) = rel_parts.last_mut() {
            if let Some(stripped) = last.strip_suffix(".jsonl") {
                *last = stripped.to_string();
            }
        }
        let sub_key =
            SessionKey::with_subpath(project_key.clone(), session_id, rel_parts.join("/"));
        append_jsonl_file_in_batches(&file_path, &sub_key, store, batch_size).await?;

        // The on-disk .jsonl does NOT contain agent_metadata entries — those
        // are only sent to live mirrors and persisted in the .meta.json
        // sidecar. Import the sidecar so resume materialization can recreate
        // it and resumed subagents keep their agentType/worktreePath. A
        // missing, corrupt, or non-object sidecar is treated as absent (the
        // transcript is still imported).
        if let Some(meta) = read_agent_metadata_sidecar(&file_path) {
            let mut meta_entry: SessionStoreEntry = meta;
            // Synthetic discriminator last so a stray "type" key in the
            // CLI-owned sidecar can never shadow it.
            meta_entry.insert(
                "type".to_string(),
                Value::String("agent_metadata".to_string()),
            );
            store.append(&sub_key, vec![meta_entry]).await?;
        }
    }
    Ok(())
}

/// Stream-read a JSONL file line-by-line, parsing each line and flushing to
/// `store.append()` in batches of `batch_size` entries (or
/// [`MAX_PENDING_BYTES`] of line text, whichever comes first). Skips blank
/// lines.
async fn append_jsonl_file_in_batches(
    file_path: &Path,
    key: &SessionKey,
    store: &Arc<dyn SessionStore>,
    batch_size: usize,
) -> Result<()> {
    let content = tokio::fs::read_to_string(file_path).await?;
    let mut batch: Vec<SessionStoreEntry> = Vec::new();
    let mut nbytes = 0usize;
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line).map_err(|e| ClaudeSdkError::JsonDecode {
            line: line.to_string(),
            original_error: e.to_string(),
        })?;
        if let Value::Object(map) = entry {
            batch.push(map);
        }
        nbytes += line.len();
        if batch.len() >= batch_size || nbytes >= MAX_PENDING_BYTES {
            store.append(key, std::mem::take(&mut batch)).await?;
            nbytes = 0;
        }
    }
    if !batch.is_empty() {
        store.append(key, batch).await?;
    }
    Ok(())
}

/// Recursively collect all `*.jsonl` file paths under `base_dir`.
///
/// Returns nothing if `base_dir` does not exist. Sorted per directory so
/// import order is deterministic across platforms.
fn collect_jsonl_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return results;
    };
    let mut dirents: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirents.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
    for entry in dirents {
        if entry.is_dir() {
            results.extend(collect_jsonl_files(&entry));
        } else if entry.is_file()
            && entry
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".jsonl"))
        {
            results.push(entry);
        }
    }
    results
}
