//! Portable session mutation functions for the Agent SDK.
//!
//! Rename/tag append typed metadata entries to the session's JSONL (matching
//! the CLI pattern); delete removes the JSONL file; fork creates a new
//! session with UUID remapping. Safe to call from any SDK host process — if
//! the target session is currently open in a CLI process, the CLI's
//! metadata re-append tail-reads before re-appending its cached metadata, so
//! an SDK write in the tail scan window is absorbed rather than clobbered.
//!
//! Directory resolution matches [`crate::list_sessions`] /
//! [`crate::get_session_messages`]: `directory` is the project path (not the
//! storage dir); when omitted, all project directories are searched for the
//! session file.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::errors::{ClaudeSdkError, Result};
use crate::sessions::{
    canonicalize_path, extract_first_prompt_from_head, extract_last_json_string_field,
    find_project_dir, get_projects_dir, get_worktree_paths, is_valid_uuid,
    project_key_for_directory, LITE_READ_BUF_SIZE,
};
use crate::types::{SessionKey, SessionStore, SessionStoreEntry, SessionStoreMethod};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Rename a session by appending a custom-title entry.
///
/// [`crate::list_sessions`] reads the LAST custom-title from the file tail,
/// so repeated calls are safe — the most recent wins.
///
/// # Errors
///
/// [`ClaudeSdkError::InvalidConfig`] if `session_id` is not a valid UUID or
/// `title` is empty/whitespace-only; [`ClaudeSdkError::SessionNotFound`] if
/// the session file cannot be found.
///
/// See [`rename_session_via_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn rename_session(session_id: &str, title: &str, directory: Option<&str>) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    // Matches the CLI guard — empty/whitespace titles are rejected rather
    // than overloaded as "clear title".
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeSdkError::InvalidConfig(
            "title must be non-empty".to_string(),
        ));
    }
    let data = format!(
        "{}\n",
        json!({
            "type": "custom-title",
            "customTitle": stripped,
            "sessionId": session_id,
        })
    );
    append_to_session(session_id, &data, directory)
}

/// Tag a session. Pass `None` to clear the tag.
///
/// Appends a `{type:'tag',tag:<tag>,sessionId:<id>}` JSONL entry.
/// [`crate::list_sessions`] reads the LAST tag from the file tail — most
/// recent wins. Passing `None` appends an empty-string tag entry which
/// listing treats as cleared. Tags are Unicode-sanitized before storing
/// (removes zero-width chars, directional marks, private-use characters,
/// etc.) for CLI filter compatibility.
///
/// # Errors
///
/// [`ClaudeSdkError::InvalidConfig`] if `session_id` is not a valid UUID or
/// `tag` is empty/whitespace-only after sanitization;
/// [`ClaudeSdkError::SessionNotFound`] if the session file cannot be found.
///
/// See [`tag_session_via_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn tag_session(session_id: &str, tag: Option<&str>, directory: Option<&str>) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let sanitized_tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag).trim().to_string();
            if sanitized.is_empty() {
                return Err(ClaudeSdkError::InvalidConfig(
                    "tag must be non-empty (use None to clear)".to_string(),
                ));
            }
            sanitized
        }
        None => String::new(),
    };
    let data = format!(
        "{}\n",
        json!({
            "type": "tag",
            "tag": sanitized_tag,
            "sessionId": session_id,
        })
    );
    append_to_session(session_id, &data, directory)
}

/// Delete a session by removing its JSONL file and subagent transcripts.
///
/// This is a hard delete — the `{session_id}.jsonl` file is removed
/// permanently, along with the sibling `{session_id}/` subdirectory that
/// holds subagent transcripts (if it exists). SDK users who need soft-delete
/// semantics can use `tag_session(id, Some("__hidden"), ..)` and filter on
/// listing instead.
///
/// # Errors
///
/// [`ClaudeSdkError::InvalidConfig`] if `session_id` is not a valid UUID;
/// [`ClaudeSdkError::SessionNotFound`] if the session file cannot be found.
///
/// See [`delete_session_via_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn delete_session(session_id: &str, directory: Option<&str>) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let Some(path) = find_session_file(session_id, directory) else {
        let suffix = directory
            .map(|d| format!(" in project directory for {d}"))
            .unwrap_or_default();
        return Err(ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found{suffix}"
        )));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ClaudeSdkError::SessionNotFound(format!(
                "Session {session_id} not found"
            )));
        }
        Err(e) => return Err(e.into()),
    }
    // Subagent transcripts live in a sibling {session_id}/ dir; often absent.
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent.join(session_id));
    }
    Ok(())
}

/// Result of a fork operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSessionResult {
    /// UUID of the new forked session.
    pub session_id: String,
}

/// Fork a session into a new branch with fresh UUIDs.
///
/// Copies transcript messages from the source session into a new session
/// file, remapping every message UUID and preserving the `parentUuid` chain.
/// Supports `up_to_message_id` for branching from a specific point in the
/// conversation. Forked sessions start without undo history (file-history
/// snapshots are not copied).
///
/// # Errors
///
/// [`ClaudeSdkError::InvalidConfig`] if `session_id` or `up_to_message_id`
/// is not a valid UUID, the session has no messages to fork, or
/// `up_to_message_id` is not found in the transcript;
/// [`ClaudeSdkError::SessionNotFound`] if the source session file cannot be
/// found.
///
/// See [`fork_session_via_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn fork_session(
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if let Some(up_to) = up_to_message_id {
        if !is_valid_uuid(up_to) {
            return Err(ClaudeSdkError::InvalidConfig(format!(
                "Invalid up_to_message_id: {up_to}"
            )));
        }
    }

    let Some((file_path, project_dir)) = find_session_file_with_dir(session_id, directory) else {
        let suffix = directory
            .map(|d| format!(" in project directory for {d}"))
            .unwrap_or_default();
        return Err(ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found{suffix}"
        )));
    };

    let content = std::fs::read(&file_path)?;
    if content.is_empty() {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Session {session_id} has no messages to fork"
        )));
    }

    let (transcript, content_replacements) = parse_fork_transcript(&content, session_id);

    let derive_title = || -> Option<String> {
        let head =
            String::from_utf8_lossy(&content[..content.len().min(LITE_READ_BUF_SIZE)]).to_string();
        let tail =
            String::from_utf8_lossy(&content[content.len().saturating_sub(LITE_READ_BUF_SIZE)..])
                .to_string();
        extract_last_json_string_field(&tail, "customTitle")
            .or_else(|| extract_last_json_string_field(&head, "customTitle"))
            .or_else(|| extract_last_json_string_field(&tail, "aiTitle"))
            .or_else(|| extract_last_json_string_field(&head, "aiTitle"))
            .or_else(|| Some(extract_first_prompt_from_head(&head)).filter(|s| !s.is_empty()))
    };

    let (forked_session_id, lines) = build_fork_lines(
        transcript,
        content_replacements,
        session_id,
        up_to_message_id,
        title,
        derive_title,
    )?;

    let fork_path = project_dir.join(format!("{forked_session_id}.jsonl"));
    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        std::os::unix::fs::OpenOptionsExt::mode(&mut open_options, 0o600);
    }
    let mut file = open_options.open(&fork_path)?;
    file.write_all(format!("{}\n", lines.join("\n")).as_bytes())?;

    Ok(ForkSessionResult {
        session_id: forked_session_id,
    })
}

/// Core fork transform — remap UUIDs and produce serialized JSONL lines.
///
/// Shared by the filesystem and [`SessionStore`]-backed paths. Returns
/// `(forked_session_id, lines)` where each line is a compact JSON string
/// without a trailing newline. `derive_title` is invoked only when no
/// explicit `title` is given.
fn build_fork_lines(
    transcript: Vec<Map<String, Value>>,
    content_replacements: Vec<Value>,
    session_id: &str,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
    derive_title: impl FnOnce() -> Option<String>,
) -> Result<(String, Vec<String>)> {
    // Filter out sidechains (subagent sessions with separate parentUuid
    // graphs). Keep isMeta entries — they're interleaved in the main chain.
    let mut transcript: Vec<Map<String, Value>> = transcript
        .into_iter()
        .filter(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .collect();

    if transcript.is_empty() {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Session {session_id} has no messages to fork"
        )));
    }

    if let Some(up_to) = up_to_message_id {
        let cutoff = transcript
            .iter()
            .position(|e| e.get("uuid").and_then(Value::as_str) == Some(up_to));
        match cutoff {
            Some(cutoff) => transcript.truncate(cutoff + 1),
            None => {
                return Err(ClaudeSdkError::InvalidConfig(format!(
                    "Message {up_to} not found in session {session_id}"
                )));
            }
        }
    }

    // Include progress entries in the mapping — needed for the parentUuid
    // chain walk.
    let mut uuid_mapping: HashMap<String, String> = HashMap::new();
    for entry in &transcript {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            uuid_mapping.insert(uuid.to_string(), uuid::Uuid::new_v4().to_string());
        }
    }

    // Filter progress messages out of the written output. They're UI-only
    // chain links; not needed in a fresh fork.
    let writable: Vec<&Map<String, Value>> = transcript
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) != Some("progress"))
        .collect();
    if writable.is_empty() {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Session {session_id} has no messages to fork"
        )));
    }

    let by_uuid: HashMap<&str, &Map<String, Value>> = transcript
        .iter()
        .filter_map(|e| e.get("uuid").and_then(Value::as_str).map(|u| (u, e)))
        .collect();

    let forked_session_id = uuid::Uuid::new_v4().to_string();
    let now = iso_now();
    let mut lines = Vec::new();

    for (i, original) in writable.iter().enumerate() {
        let original_uuid = original
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let new_uuid = uuid_mapping
            .get(original_uuid)
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Resolve parentUuid, skipping progress ancestors.
        let mut new_parent_uuid: Option<String> = None;
        let mut parent_id = original
            .get("parentUuid")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        while let Some(pid) = parent_id {
            let Some(parent) = by_uuid.get(pid.as_str()) else {
                break;
            };
            if parent.get("type").and_then(Value::as_str) != Some("progress") {
                new_parent_uuid = uuid_mapping.get(&pid).cloned();
                break;
            }
            parent_id = parent
                .get("parentUuid")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .map(str::to_string);
        }

        // Only update the timestamp on the last message (leaf detection on
        // resume).
        let timestamp = if i == writable.len() - 1 {
            Value::String(now.clone())
        } else {
            original
                .get("timestamp")
                .cloned()
                .unwrap_or_else(|| Value::String(now.clone()))
        };

        // Remap logicalParentUuid (compact-boundary backpointer).
        let new_logical_parent = match original.get("logicalParentUuid") {
            Some(Value::String(lp)) if !lp.is_empty() => uuid_mapping
                .get(lp)
                .cloned()
                .map(Value::String)
                .unwrap_or(Value::Null),
            Some(other) => other.clone(),
            None => Value::Null,
        };

        let mut forked = (*original).clone();
        forked.insert("uuid".to_string(), Value::String(new_uuid));
        forked.insert(
            "parentUuid".to_string(),
            new_parent_uuid.map(Value::String).unwrap_or(Value::Null),
        );
        forked.insert("logicalParentUuid".to_string(), new_logical_parent);
        forked.insert(
            "sessionId".to_string(),
            Value::String(forked_session_id.clone()),
        );
        forked.insert("timestamp".to_string(), timestamp);
        // Clear session-specific fields.
        forked.insert("isSidechain".to_string(), Value::Bool(false));
        forked.insert(
            "forkedFrom".to_string(),
            json!({
                "sessionId": session_id,
                "messageUuid": original_uuid,
            }),
        );
        // Remove fields that would leak state from the source session.
        for key in ["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
            forked.remove(key);
        }
        lines.push(Value::Object(forked).to_string());
    }

    // Append a content-replacement entry (if any) with the fork's sessionId.
    if !content_replacements.is_empty() {
        lines.push(
            json!({
                "type": "content-replacement",
                "sessionId": forked_session_id,
                "replacements": content_replacements,
                "uuid": uuid::Uuid::new_v4().to_string(),
                "timestamp": now,
            })
            .to_string(),
        );
    }

    // Derive the title: explicit > original customTitle > original aiTitle >
    // first prompt. Suffix with " (fork)" for derived titles. Listing reads
    // the LAST custom-title from the tail, so this entry is what surfaces.
    let fork_title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{} (fork)",
                derive_title().unwrap_or_else(|| "Forked session".to_string())
            )
        });
    lines.push(
        json!({
            "type": "custom-title",
            "sessionId": forked_session_id,
            "customTitle": fork_title,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "timestamp": now,
        })
        .to_string(),
    );

    Ok((forked_session_id, lines))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_session_file(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    find_session_file_with_dir(session_id, directory).map(|(path, _)| path)
}

/// Find a session file and its containing project directory.
///
/// Returns `(file_path, project_dir)`. The fork operation needs the project
/// dir to write the new file adjacent to the source.
fn find_session_file_with_dir(
    session_id: &str,
    directory: Option<&str>,
) -> Option<(PathBuf, PathBuf)> {
    let file_name = format!("{session_id}.jsonl");

    let try_dir = |project_dir: &Path| -> Option<(PathBuf, PathBuf)> {
        let path = project_dir.join(&file_name);
        match path.metadata() {
            Ok(metadata) if metadata.len() > 0 => Some((path, project_dir.to_path_buf())),
            _ => None,
        }
    };

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if let Some(result) = try_dir(&project_dir) {
                return Some(result);
            }
        }
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt) {
                if let Some(result) = try_dir(&wt_project_dir) {
                    return Some(result);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        if let Some(result) = try_dir(&entry.path()) {
            return Some(result);
        }
    }
    None
}

const TRANSCRIPT_TYPES: [&str; 5] = ["user", "assistant", "attachment", "system", "progress"];

/// Mirror the disk path's head/tail title scan over parsed entry objects.
///
/// Precedence matches [`extract_last_json_string_field`] semantics: last
/// occurrence wins for both `customTitle` and `aiTitle`; `customTitle` beats
/// `aiTitle`; first user prompt is the final fallback.
fn derive_title_from_entries(raw: &[SessionStoreEntry]) -> Option<String> {
    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    for e in raw {
        if let Some(ct) = e
            .get("customTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            custom = Some(ct.to_string());
        }
        if let Some(at) = e
            .get("aiTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            ai = Some(at.to_string());
        }
    }
    if custom.is_some() {
        return custom;
    }
    if ai.is_some() {
        return ai;
    }
    // First-prompt fallback — reuse the head extractor over a re-serialized
    // JSONL string so skip-patterns/truncation match the disk path exactly.
    let jsonl: String = raw
        .iter()
        .map(|e| format!("{}\n", Value::Object(e.clone())))
        .collect();
    Some(extract_first_prompt_from_head(&jsonl)).filter(|s| !s.is_empty())
}

/// Parse JSONL content into transcript entries + content-replacement
/// records. Only keeps entries that have a uuid and are transcript message
/// types. Content-replacement entries are collected for re-emission in the
/// fork.
fn parse_fork_transcript(
    content: &[u8],
    session_id: &str,
) -> (Vec<Map<String, Value>>, Vec<Value>) {
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();

    for line in String::from_utf8_lossy(content).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if TRANSCRIPT_TYPES.contains(&entry_type) && entry.get("uuid").is_some_and(Value::is_string)
        {
            transcript.push(entry);
        } else if entry_type == "content-replacement"
            && entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            if let Some(replacements) = entry.get("replacements").and_then(Value::as_array) {
                content_replacements.extend(replacements.iter().cloned());
            }
        }
    }
    (transcript, content_replacements)
}

/// Append data to an existing session file.
///
/// Searches candidate paths and tries the append directly — no existence
/// check. Opens append-only without create, so the open fails for missing
/// files, avoiding TOCTOU. A 0-byte `.jsonl` is a "session not here, keep
/// searching" signal that readers already honor.
fn append_to_session(session_id: &str, data: &str, directory: Option<&str>) -> Result<()> {
    let file_name = format!("{session_id}.jsonl");

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);

        if let Some(project_dir) = find_project_dir(&canonical) {
            if try_append(&project_dir.join(&file_name), data)? {
                return Ok(());
            }
        }
        // Worktree fallback — sessions may live under a different worktree
        // root.
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt) {
                if try_append(&wt_project_dir.join(&file_name), data)? {
                    return Ok(());
                }
            }
        }
        return Err(ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found in project directory for {directory}"
        )));
    }

    // No directory — search all project directories by trying each directly.
    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).map_err(|_| {
        ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found (no projects directory)"
        ))
    })?;
    for entry in entries.flatten() {
        if try_append(&entry.path().join(&file_name), data)? {
            return Ok(());
        }
    }
    Err(ClaudeSdkError::SessionNotFound(format!(
        "Session {session_id} not found in any project directory"
    )))
}

/// Try appending to a path.
///
/// Opens append-only without create, so the open fails if the file does not
/// exist — no separate existence check. Returns `Ok(true)` on a successful
/// write, `Ok(false)` if the file does not exist or is 0-byte (a "session
/// not here, keep searching" signal — without this guard the search would
/// stop at an empty stub in one project dir while the real file lives in a
/// worktree). Other errors (out of space, permissions, I/O) propagate so
/// real write failures surface. Kernel append mode makes each write
/// atomically seek-to-EOF (race-free) on all supported platforms.
fn try_append(path: &Path, data: &str) -> Result<bool> {
    let open_result = std::fs::OpenOptions::new().append(true).open(path);
    let mut file = match open_result {
        Ok(file) => file,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.write_all(data.as_bytes())?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Unicode sanitization
// ---------------------------------------------------------------------------

/// Whether `c` is a dangerous character stripped from tags: zero-width and
/// directional marks, BOM, private-use characters, and common format
/// characters.
fn is_stripped_char(c: char) -> bool {
    matches!(c,
        '\u{00ad}'                     // soft hyphen
        | '\u{061c}'                   // Arabic letter mark
        | '\u{200b}'..='\u{200f}'      // zero-width spaces, LTR/RTL marks
        | '\u{202a}'..='\u{202e}'      // directional formatting characters
        | '\u{2060}'..='\u{2064}'      // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}'      // directional isolates
        | '\u{feff}'                   // byte order mark
        | '\u{fff9}'..='\u{fffb}'      // interlinear annotation
        | '\u{e000}'..='\u{f8ff}'      // BMP private use
        | '\u{e0000}'..='\u{e007f}'    // tag characters
        | '\u{f0000}'..='\u{ffffd}'    // supplementary private use A
        | '\u{100000}'..='\u{10fffd}'  // supplementary private use B
    )
}

/// Sanitize a string by removing dangerous Unicode characters.
///
/// Iteratively applies NFKC normalization and strips
/// format/private-use/invisible characters until no more changes occur (max
/// 10 iterations). Mirrors the TypeScript SDK's explicit-range fallback.
fn sanitize_unicode(value: &str) -> String {
    let mut current = value.to_string();
    for _ in 0..10 {
        let previous = current.clone();
        current = current.nfkc().filter(|c| !is_stripped_char(*c)).collect();
        if current == previous {
            break;
        }
    }
    current
}

// ---------------------------------------------------------------------------
// SessionStore-backed implementations
// ---------------------------------------------------------------------------

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn as_store_entry(value: Value) -> SessionStoreEntry {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Rename a session by appending a custom-title entry to a [`SessionStore`].
///
/// Async, store-backed counterpart to [`rename_session`].
pub async fn rename_session_via_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    title: &str,
    directory: Option<&str>,
) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeSdkError::InvalidConfig(
            "title must be non-empty".to_string(),
        ));
    }
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    let entry = as_store_entry(json!({
        "type": "custom-title",
        "customTitle": stripped,
        "sessionId": session_id,
        "uuid": uuid::Uuid::new_v4().to_string(),
        "timestamp": iso_now(),
    }));
    session_store.append(&key, vec![entry]).await
}

/// Tag a session by appending a tag entry to a [`SessionStore`].
///
/// Async, store-backed counterpart to [`tag_session`]. Pass `None` to clear
/// the tag. Tags are Unicode-sanitized before storing.
pub async fn tag_session_via_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    tag: Option<&str>,
    directory: Option<&str>,
) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let sanitized_tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag).trim().to_string();
            if sanitized.is_empty() {
                return Err(ClaudeSdkError::InvalidConfig(
                    "tag must be non-empty (use None to clear)".to_string(),
                ));
            }
            sanitized
        }
        None => String::new(),
    };
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    let entry = as_store_entry(json!({
        "type": "tag",
        "tag": sanitized_tag,
        "sessionId": session_id,
        "uuid": uuid::Uuid::new_v4().to_string(),
        "timestamp": iso_now(),
    }));
    session_store.append(&key, vec![entry]).await
}

/// Delete a session from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`delete_session`]. If the store does
/// not implement [`SessionStore::delete`], deletion is a no-op (appropriate
/// for WORM/append-only backends — matches the [`SessionStore`] contract).
/// Whether subagent transcripts under the session are also removed depends
/// on the store's delete semantics — [`crate::InMemorySessionStore`]
/// cascades; custom stores may not.
pub async fn delete_session_via_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
) -> Result<()> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if !session_store.implements(SessionStoreMethod::Delete) {
        return Ok(());
    }
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    session_store.delete(&key).await
}

/// Fork a session into a new branch with fresh UUIDs via a [`SessionStore`].
///
/// Async, store-backed counterpart to [`fork_session`]. Runs the fork
/// transform directly over the objects returned by [`SessionStore::load`] —
/// no JSONL round-trip. A storage-layer copy (e.g. S3 CopyObject) is NOT
/// sufficient: the transform remaps every UUID, rewrites `sessionId` on each
/// entry, and stamps `forkedFrom`, so the data must pass through this
/// process once.
pub async fn fork_session_via_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if !is_valid_uuid(session_id) {
        return Err(ClaudeSdkError::InvalidConfig(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if let Some(up_to) = up_to_message_id {
        if !is_valid_uuid(up_to) {
            return Err(ClaudeSdkError::InvalidConfig(format!(
                "Invalid up_to_message_id: {up_to}"
            )));
        }
    }
    let project_key = project_key_for_directory(directory);
    let src_key = SessionKey::new(project_key.clone(), session_id);
    let loaded = session_store.load(&src_key).await?.unwrap_or_default();
    if loaded.is_empty() {
        return Err(ClaudeSdkError::SessionNotFound(format!(
            "Session {session_id} not found"
        )));
    }

    // Partition into transcript entries (with uuid) and content-replacement
    // records, mirroring parse_fork_transcript for the already-parsed path.
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();
    for entry in &loaded {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if TRANSCRIPT_TYPES.contains(&entry_type) && entry.get("uuid").is_some_and(Value::is_string)
        {
            transcript.push(entry.clone());
        } else if entry_type == "content-replacement"
            && entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            if let Some(replacements) = entry.get("replacements").and_then(Value::as_array) {
                content_replacements.extend(replacements.iter().cloned());
            }
        }
    }

    let (forked_session_id, lines) = build_fork_lines(
        transcript,
        content_replacements,
        session_id,
        up_to_message_id,
        title,
        || derive_title_from_entries(&loaded),
    )?;

    let dst_key = SessionKey::new(project_key, forked_session_id.clone());
    // build_fork_lines emits compact JSON strings; re-parse to objects so
    // the store receives the same shape it would from the mirror path.
    let entries: Vec<SessionStoreEntry> = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(as_store_entry)
        .collect();
    session_store.append(&dst_key, entries).await?;
    Ok(ForkSessionResult {
        session_id: forked_session_id,
    })
}
