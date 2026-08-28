//! Incremental session-summary derivation for [`SessionStore`] adapters.
//!
//! [`fold_session_summary`] lets a store maintain a per-session
//! [`SessionSummaryEntry`] sidecar incrementally inside `append()` so
//! [`crate::list_sessions_from_store`] can fetch all metadata in a single
//! `list_session_summaries()` call instead of N per-session `load()` calls.
//!
//! Every derived field is append-incremental (set-once or last-wins) so
//! adapters never need to re-read previously appended entries.

use serde_json::{Map, Value};

use crate::sessions::{extract_command_name, iso_to_epoch_ms, matches_skip_first_prompt_pattern};
use crate::types::{SdkSessionInfo, SessionKey, SessionStoreEntry, SessionSummaryEntry};

// Referenced by doc comments.
#[allow(unused_imports)]
use crate::types::SessionStore;

/// Map of JSONL entry keys → summary data keys for last-wins string fields.
/// Each appended entry overwrites the previous value when present.
const LAST_WINS_FIELDS: [(&str, &str); 5] = [
    ("customTitle", "custom_title"),
    ("aiTitle", "ai_title"),
    ("lastPrompt", "last_prompt"),
    ("summary", "summary_hint"),
    ("gitBranch", "git_branch"),
];

/// Extract text strings from a `type == "user"` entry's message content.
fn entry_text_blocks(entry: &Map<String, Value>) -> Vec<String> {
    let Some(message) = entry.get("message").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut texts = Vec::new();
    match message.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        texts.push(text.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    texts
}

/// Replicate the head first-prompt scan for a single parsed entry.
///
/// Mutates `data` in place: sets `first_prompt` + `first_prompt_locked` on a
/// real match, or stashes a `command_fallback` for slash-command messages.
/// Skips tool_result, isMeta, isCompactSummary, and auto-generated patterns.
fn fold_first_prompt(data: &mut Map<String, Value>, entry: &Map<String, Value>) {
    if data.get("first_prompt_locked").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return;
    }
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        || entry.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    // Skip tool_result-carrying user messages.
    if let Some(content) = entry
        .get("message")
        .and_then(Value::as_object)
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        if content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            return;
        }
    }

    for raw in entry_text_blocks(entry) {
        let result = raw.replace('\n', " ").trim().to_string();
        if result.is_empty() {
            continue;
        }
        if let Some(cmd) = extract_command_name(&result) {
            let fallback_empty = data
                .get("command_fallback")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty);
            if fallback_empty {
                data.insert("command_fallback".to_string(), Value::String(cmd));
            }
            continue;
        }
        if matches_skip_first_prompt_pattern(&result) {
            continue;
        }
        let result = if result.chars().count() > 200 {
            let truncated: String = result.chars().take(200).collect();
            format!("{}\u{2026}", truncated.trim_end())
        } else {
            result
        };
        data.insert("first_prompt".to_string(), Value::String(result));
        data.insert("first_prompt_locked".to_string(), Value::Bool(true));
        return;
    }
}

/// Fold a batch of appended entries into the running summary for `key`.
///
/// Stores call this from inside `append()` to keep a
/// [`SessionSummaryEntry`] sidecar up to date without re-reading the
/// transcript. `prev` is the previous summary for the same key (or `None`
/// for the first append).
///
/// Do not call this for keys with a `subpath` — subagent transcripts must
/// not contribute to the main session's summary. Guard with
/// `key.subpath.is_none()` before calling.
///
/// All derived state lives in the opaque `data` map; stores persist it
/// verbatim and do not interpret it.
///
/// `mtime` is NOT touched by the fold — it is the sidecar's storage write
/// time and must be stamped by the adapter after persisting. It has to share
/// a clock with the `mtime` returned by [`SessionStore::list_sessions`] for
/// the same session; deriving it from entry ISO timestamps would make every
/// batched-write sidecar appear strictly older than the session's current
/// mtime, defeating the fast-path staleness check. For a new session (`prev
/// is None`) the fold returns `mtime = 0` as a placeholder; the adapter is
/// expected to overwrite it.
///
/// `created_at` latches the first parseable entry timestamp, agreeing with
/// the disk lite-parse for any timestamp appearing within the head window.
pub fn fold_session_summary(
    prev: Option<&SessionSummaryEntry>,
    key: &SessionKey,
    entries: &[SessionStoreEntry],
) -> SessionSummaryEntry {
    let mut summary = match prev {
        Some(prev) => prev.clone(),
        None => SessionSummaryEntry {
            session_id: key.session_id.clone(),
            mtime: 0,
            data: Map::new(),
        },
    };
    let data = &mut summary.data;

    for entry in entries {
        let ms = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso_to_epoch_ms);

        if !data.contains_key("is_sidechain") {
            data.insert(
                "is_sidechain".to_string(),
                Value::Bool(entry.get("isSidechain").and_then(Value::as_bool) == Some(true)),
            );
        }
        if !data.contains_key("created_at") {
            if let Some(ms) = ms {
                data.insert("created_at".to_string(), Value::from(ms));
            }
        }
        if !data.contains_key("cwd") {
            if let Some(cwd) = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                data.insert("cwd".to_string(), Value::String(cwd.to_string()));
            }
        }

        fold_first_prompt(data, entry);

        for (src, dst) in LAST_WINS_FIELDS {
            if let Some(val) = entry.get(src).and_then(Value::as_str) {
                data.insert(dst.to_string(), Value::String(val.to_string()));
            }
        }

        if entry.get("type").and_then(Value::as_str) == Some("tag") {
            match entry
                .get("tag")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                Some(tag) => {
                    data.insert("tag".to_string(), Value::String(tag.to_string()));
                }
                None => {
                    // Empty string or absent tag clears the tag.
                    data.remove("tag");
                }
            }
        }
    }

    summary
}

/// Convert a [`SessionSummaryEntry`] to [`SdkSessionInfo`].
///
/// Returns `None` for sidechain sessions or sessions with no extractable
/// summary, matching the lite-parse filtering.
pub fn summary_entry_to_sdk_info(
    entry: &SessionSummaryEntry,
    project_path: Option<&str>,
) -> Option<SdkSessionInfo> {
    let data = &entry.data;
    if data.get("is_sidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let get = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let first_prompt = if data.get("first_prompt_locked").and_then(Value::as_bool) == Some(true) {
        get("first_prompt")
    } else {
        get("command_fallback")
    };
    let custom_title = get("custom_title").or_else(|| get("ai_title"));
    let summary = custom_title
        .clone()
        .or_else(|| get("last_prompt"))
        .or_else(|| get("summary_hint"))
        .or_else(|| first_prompt.clone())?;

    Some(SdkSessionInfo {
        session_id: entry.session_id.clone(),
        summary,
        last_modified: entry.mtime,
        // file_size is a JSONL byte count — meaningful only for the
        // local-disk path. Stores have no equivalent.
        file_size: None,
        custom_title,
        first_prompt,
        git_branch: get("git_branch"),
        cwd: get("cwd").or_else(|| project_path.map(str::to_string)),
        tag: get("tag"),
        created_at: data.get("created_at").and_then(Value::as_i64),
    })
}
