//! Session listing implementation.
//!
//! Scans `~/.claude/projects/<sanitized-cwd>/` for `.jsonl` session files and
//! extracts metadata from stat + head/tail reads without full JSONL parsing,
//! plus [`SessionStore`]-backed async variants of every listing function.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{
    SdkSessionInfo, SessionKey, SessionListSubkeysKey, SessionMessage, SessionMessageType,
    SessionStore, SessionStoreEntry, SessionStoreMethod,
};

/// Size of the head/tail buffer for lite metadata reads.
pub(crate) const LITE_READ_BUF_SIZE: usize = 65536;

/// Upper bound on concurrent `store.load()` calls issued by
/// [`list_sessions_from_store`]. Keeps large project listings from
/// exhausting adapter connection pools or tripping backend rate limits.
const STORE_LIST_LOAD_CONCURRENCY: usize = 16;

/// Maximum length for a single filesystem path component. Most filesystems
/// limit individual components to 255 bytes; 200 leaves room for the hash
/// suffix and separator.
pub(crate) const MAX_SANITIZED_LENGTH: usize = 200;

// ---------------------------------------------------------------------------
// UUID validation
// ---------------------------------------------------------------------------

/// Whether `maybe_uuid` is a valid `8-4-4-4-12` hex UUID.
pub(crate) fn is_valid_uuid(maybe_uuid: &str) -> bool {
    let parts: Vec<&str> = maybe_uuid.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lengths)
            .all(|(part, len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// Path sanitization
// ---------------------------------------------------------------------------

/// 32-bit integer hash to base36, matching the CLI's directory naming.
fn simple_hash(s: &str) -> String {
    let mut h: i32 = 0;
    for ch in s.chars() {
        h = (h << 5).wrapping_sub(h).wrapping_add(ch as i32);
    }
    let mut n = (h as i64).unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ASCII")
}

/// Make a string safe for use as a directory name.
///
/// Replaces all non-alphanumeric characters with hyphens. For paths
/// exceeding [`MAX_SANITIZED_LENGTH`], truncates and appends a hash suffix.
pub(crate) fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let truncated: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    format!("{truncated}-{}", simple_hash(name))
}

// ---------------------------------------------------------------------------
// Config directories
// ---------------------------------------------------------------------------

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// The Claude config directory (respects `CLAUDE_CONFIG_DIR`).
fn get_claude_config_home_dir() -> PathBuf {
    if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !config_dir.is_empty() {
            return PathBuf::from(nfc(&config_dir));
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    PathBuf::from(nfc(&home.join(".claude").to_string_lossy()))
}

/// The projects directory.
///
/// `env_override` is consulted before the process environment so callers
/// that pass `CLAUDE_CONFIG_DIR` to the subprocess via `options.env` resolve
/// the same directory the subprocess will write to.
pub(crate) fn get_projects_dir(env_override: Option<&HashMap<String, String>>) -> PathBuf {
    if let Some(env) = env_override {
        if let Some(dir) = env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(nfc(dir)).join("projects");
        }
    }
    get_claude_config_home_dir().join("projects")
}

fn get_project_dir(project_path: &str) -> PathBuf {
    get_projects_dir(None).join(sanitize_path(project_path))
}

/// Resolve a directory path to its canonical form using realpath + NFC.
pub(crate) fn canonicalize_path(d: &str) -> String {
    match std::fs::canonicalize(d) {
        Ok(resolved) => nfc(&resolved.to_string_lossy()),
        Err(_) => nfc(d),
    }
}

/// Find the project directory for a given path.
///
/// Tolerates hash mismatches for long paths (>200 chars): different runtimes
/// produce different directory suffixes, so for paths that exceed
/// [`MAX_SANITIZED_LENGTH`] this falls back to prefix-based scanning when
/// the exact match doesn't exist.
pub(crate) fn find_project_dir(project_path: &str) -> Option<PathBuf> {
    let exact = get_project_dir(project_path);
    if exact.is_dir() {
        return Some(exact);
    }

    // Exact match failed — for short paths this means no sessions exist. For
    // long paths, try prefix matching to handle hash mismatches.
    let sanitized = sanitize_path(project_path);
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return None;
    }
    let prefix: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    let prefix = format!("{prefix}-");
    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        {
            return Some(path);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JSON string field extraction — no full parse, works on truncated lines
// ---------------------------------------------------------------------------

/// Unescape a JSON string value extracted as raw text.
fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    match serde_json::from_str::<Value>(&format!("\"{raw}\"")) {
        Ok(Value::String(s)) => s,
        _ => raw.to_string(),
    }
}

/// Scan `text[value_start..]` for the closing quote of a JSON string,
/// honoring backslash escapes. Returns the raw span and the index just past
/// the closing quote, byte-indexed.
fn scan_string_value(text: &str, value_start: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = value_start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((i, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Extract a simple JSON string field value without full parsing.
///
/// Looks for `"key":"value"` or `"key": "value"` patterns. Returns the first
/// match, or `None` if not found.
pub(crate) fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let Some(idx) = text.find(&pattern) else {
            continue;
        };
        let value_start = idx + pattern.len();
        if let Some((end, _)) = scan_string_value(text, value_start) {
            return Some(unescape_json_string(&text[value_start..end]));
        }
    }
    None
}

/// Like [`extract_json_string_field`] but finds the LAST occurrence.
pub(crate) fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    let mut last_value: Option<String> = None;
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let mut search_from = 0;
        while let Some(rel_idx) = text[search_from..].find(&pattern) {
            let idx = search_from + rel_idx;
            let value_start = idx + pattern.len();
            match scan_string_value(text, value_start) {
                Some((end, next)) => {
                    last_value = Some(unescape_json_string(&text[value_start..end]));
                    search_from = next;
                }
                None => break,
            }
        }
    }
    last_value
}

// ---------------------------------------------------------------------------
// First prompt extraction from head chunk
// ---------------------------------------------------------------------------

/// Whether `result` matches an auto-generated or system message that should
/// be skipped when looking for the first meaningful user prompt.
pub(crate) fn matches_skip_first_prompt_pattern(result: &str) -> bool {
    for prefix in [
        "<local-command-stdout>",
        "<session-start-hook>",
        "<tick>",
        "<goal>",
    ] {
        if result.starts_with(prefix) {
            return true;
        }
    }
    if result.starts_with("[Request interrupted by user") {
        // The Python pattern requires the bracketed prefix (any non-]
        // continuation) — the prefix check is sufficient here.
        return true;
    }
    for (open, close) in [
        ("<ide_opened_file>", "</ide_opened_file>"),
        ("<ide_selection>", "</ide_selection>"),
    ] {
        let trimmed_start = result.trim_start();
        if trimmed_start.starts_with(open) && result.trim_end().ends_with(close) {
            return true;
        }
    }
    false
}

/// Extract the `<command-name>...</command-name>` value, if present.
pub(crate) fn extract_command_name(result: &str) -> Option<String> {
    let start = result.find("<command-name>")? + "<command-name>".len();
    let end = result[start..].find("</command-name>")? + start;
    Some(result[start..end].to_string())
}

/// Truncate to 200 chars with an ellipsis, mirroring the CLI's summary caps.
fn truncate_prompt(result: &str) -> String {
    if result.chars().count() > 200 {
        let truncated: String = result.chars().take(200).collect();
        format!("{}\u{2026}", truncated.trim_end())
    } else {
        result.to_string()
    }
}

/// Extract the first meaningful user prompt from a JSONL head chunk.
///
/// Skips tool_result messages, isMeta, isCompactSummary, command-name
/// messages, and auto-generated patterns. Truncates to 200 chars.
pub(crate) fn extract_first_prompt_from_head(head: &str) -> String {
    let mut command_fallback = String::new();

    for line in head.split('\n') {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(message) = entry.get("message").filter(|m| m.is_object()) else {
            continue;
        };

        let mut texts: Vec<String> = Vec::new();
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

        for raw in texts {
            let result = raw.replace('\n', " ").trim().to_string();
            if result.is_empty() {
                continue;
            }
            // Skip slash-command messages but remember the first as fallback.
            if let Some(cmd) = extract_command_name(&result) {
                if command_fallback.is_empty() {
                    command_fallback = cmd;
                }
                continue;
            }
            if matches_skip_first_prompt_pattern(&result) {
                continue;
            }
            return truncate_prompt(&result);
        }
    }

    command_fallback
}

// ---------------------------------------------------------------------------
// File I/O — read head and tail of a file
// ---------------------------------------------------------------------------

/// Result of reading a session file's head, tail, mtime and size.
pub(crate) struct LiteSessionFile {
    pub mtime: i64,
    pub size: u64,
    pub head: String,
    pub tail: String,
}

/// Open a session file, stat it, and read head + tail. Returns `None` on any
/// error or if the file is empty.
pub(crate) fn read_session_lite(file_path: &Path) -> Option<LiteSessionFile> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(file_path).ok()?;
    let metadata = file.metadata().ok()?;
    let size = metadata.len();
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;

    let mut head_bytes = vec![0u8; LITE_READ_BUF_SIZE];
    let mut read = 0;
    while read < head_bytes.len() {
        match file.read(&mut head_bytes[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    head_bytes.truncate(read);
    if head_bytes.is_empty() {
        return None;
    }
    let head = String::from_utf8_lossy(&head_bytes).to_string();

    let tail_offset = size.saturating_sub(LITE_READ_BUF_SIZE as u64);
    let tail = if tail_offset == 0 {
        head.clone()
    } else {
        file.seek(SeekFrom::Start(tail_offset)).ok()?;
        let mut tail_bytes = Vec::with_capacity(LITE_READ_BUF_SIZE);
        file.take(LITE_READ_BUF_SIZE as u64)
            .read_to_end(&mut tail_bytes)
            .ok()?;
        String::from_utf8_lossy(&tail_bytes).to_string()
    };

    Some(LiteSessionFile {
        mtime,
        size,
        head,
        tail,
    })
}

// ---------------------------------------------------------------------------
// Git worktree detection
// ---------------------------------------------------------------------------

/// How long to wait for `git worktree list` before giving up (matches the
/// Python SDK's subprocess timeout).
const GIT_WORKTREE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Absolute worktree paths for the git repo containing `cwd`. Returns an
/// empty list if git is unavailable, `cwd` is not in a repo, or the command
/// does not finish within [`GIT_WORKTREE_TIMEOUT`].
pub(crate) fn get_worktree_paths(cwd: &str) -> Vec<String> {
    worktree_paths_with_git("git", cwd)
}

/// [`get_worktree_paths`] with an explicit git executable, so tests can
/// substitute a fake.
fn worktree_paths_with_git(git: &str, cwd: &str) -> Vec<String> {
    use std::io::Read;

    let child = std::process::Command::new(git)
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Vec::new();
    };

    // Drain stdout on a separate thread so a listing larger than the pipe
    // buffer cannot block git and false-trip the timeout below (the same
    // concurrent-capture shape as Python's subprocess.run(capture_output=
    // True, timeout=5)). The reader sees EOF when git exits or is killed,
    // so joining it after the wait is bounded.
    let reader = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut stdout = Vec::new();
            let _ = pipe.read_to_end(&mut stdout);
            stdout
        })
    });

    // Bounded wait: a hung git (network filesystem, stuck lock) must not
    // stall session listing.
    let deadline = std::time::Instant::now() + GIT_WORKTREE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                // Detach the reader rather than joining: a grandchild of git
                // that inherited the pipe's write end can keep it open past
                // the kill, and joining would block until it exits. The
                // thread ends on its own at EOF.
                drop(reader);
                return Vec::new();
            }
        }
    };
    // git has exited; its output normally ends right away. Still bound the
    // join by the same deadline — a lingering grandchild of git holding the
    // pipe's write end must not stall past the timeout (detach and give up,
    // as above).
    let stdout = match reader {
        Some(reader) => {
            while !reader.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if !reader.is_finished() {
                drop(reader);
                return Vec::new();
            }
            reader.join().unwrap_or_default()
        }
        None => Vec::new(),
    };
    if !status.success() || stdout.is_empty() {
        return Vec::new();
    }
    String::from_utf8_lossy(&stdout)
        .split('\n')
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(nfc)
        .collect()
}

// ---------------------------------------------------------------------------
// Timestamp parsing
// ---------------------------------------------------------------------------

/// Parse an ISO-8601 timestamp string to Unix epoch milliseconds.
pub(crate) fn iso_to_epoch_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

// ---------------------------------------------------------------------------
// Field extraction — shared by list_sessions and get_session_info
// ---------------------------------------------------------------------------

/// Parse [`SdkSessionInfo`] fields from a lite session read (head/tail/stat).
///
/// Returns `None` for sidechain sessions or metadata-only sessions with no
/// extractable summary. Shared by [`list_sessions`] and
/// [`get_session_info`].
pub(crate) fn parse_session_info_from_lite(
    session_id: &str,
    lite: &LiteSessionFile,
    project_path: Option<&str>,
) -> Option<SdkSessionInfo> {
    let (head, tail) = (&lite.head, &lite.tail);

    // Check the first line for sidechain sessions.
    let first_line = head.split('\n').next().unwrap_or("");
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }

    // User-set title (customTitle) wins over AI-generated title (aiTitle).
    // Head fallback covers short sessions where the title entry may not be
    // in the tail.
    let custom_title = extract_last_json_string_field(tail, "customTitle")
        .or_else(|| extract_last_json_string_field(head, "customTitle"))
        .or_else(|| extract_last_json_string_field(tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(head, "aiTitle"))
        .filter(|s| !s.is_empty());
    let first_prompt = Some(extract_first_prompt_from_head(head)).filter(|s| !s.is_empty());
    // The lastPrompt tail entry shows what the user was most recently doing.
    let summary = custom_title
        .clone()
        .or_else(|| extract_last_json_string_field(tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(tail, "summary"))
        .or_else(|| first_prompt.clone())
        .filter(|s| !s.is_empty());

    // Skip metadata-only sessions (no title, no summary, no prompt).
    let summary = summary?;

    let git_branch = extract_last_json_string_field(tail, "gitBranch")
        .or_else(|| extract_json_string_field(head, "gitBranch"))
        .filter(|s| !s.is_empty());
    let session_cwd = extract_json_string_field(head, "cwd")
        .filter(|s| !s.is_empty())
        .or_else(|| project_path.map(str::to_string));
    // Scope tag extraction to {"type":"tag"} lines — a bare tail scan for
    // "tag" would match tool_use inputs (git tag, Docker tags, cloud
    // resource tags).
    let tag = tail
        .split('\n')
        .rev()
        .find(|line| line.starts_with("{\"type\":\"tag\""))
        .and_then(|line| extract_last_json_string_field(line, "tag"))
        .filter(|s| !s.is_empty());

    // created_at from the first ISO timestamp found in the head (epoch ms).
    // More reliable than filesystem birth time, which is unsupported on some
    // filesystems. Scans the whole head rather than only the first line
    // because the first record may be a metadata-only entry (e.g.
    // permission-mode) with no timestamp field.
    let created_at =
        extract_json_string_field(head, "timestamp").and_then(|ts| iso_to_epoch_ms(&ts));

    Some(SdkSessionInfo {
        session_id: session_id.to_string(),
        summary,
        last_modified: lite.mtime,
        file_size: Some(lite.size),
        custom_title,
        first_prompt,
        git_branch,
        cwd: session_cwd,
        tag,
        created_at,
    })
}

// ---------------------------------------------------------------------------
// Core implementation
// ---------------------------------------------------------------------------

/// Read session files from a single project directory.
///
/// Each file gets a stat + head/tail read. Filters out sidechain sessions
/// and metadata-only sessions (no title/summary/prompt).
fn read_sessions_from_dir(project_dir: &Path, project_path: Option<&str>) -> Vec<SdkSessionInfo> {
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(session_id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !is_valid_uuid(session_id) {
            continue;
        }
        let Some(lite) = read_session_lite(&path) else {
            continue;
        };
        if let Some(info) = parse_session_info_from_lite(session_id, &lite, project_path) {
            results.push(info);
        }
    }
    results
}

/// Deduplicate by session_id, keeping the newest `last_modified`.
fn deduplicate_by_session_id(sessions: Vec<SdkSessionInfo>) -> Vec<SdkSessionInfo> {
    let mut by_id: HashMap<String, SdkSessionInfo> = HashMap::new();
    for s in sessions {
        match by_id.get(&s.session_id) {
            Some(existing) if s.last_modified <= existing.last_modified => {}
            _ => {
                by_id.insert(s.session_id.clone(), s);
            }
        }
    }
    by_id.into_values().collect()
}

/// Sort sessions by `last_modified` descending and apply offset + limit.
fn apply_sort_limit_offset(
    mut sessions: Vec<SdkSessionInfo>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SdkSessionInfo> {
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    if offset > 0 {
        sessions = sessions.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit {
        if limit > 0 {
            sessions.truncate(limit);
        }
    }
    sessions
}

/// List sessions for a specific project directory (and its worktrees).
fn list_sessions_for_project(
    directory: &str,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SdkSessionInfo> {
    let canonical_dir = canonicalize_path(directory);

    let worktree_paths = if include_worktrees {
        get_worktree_paths(&canonical_dir)
    } else {
        Vec::new()
    };

    // No worktrees (or git not available / scanning disabled) — just scan
    // the single project dir.
    if worktree_paths.len() <= 1 {
        let Some(project_dir) = find_project_dir(&canonical_dir) else {
            return Vec::new();
        };
        let sessions = read_sessions_from_dir(&project_dir, Some(&canonical_dir));
        return apply_sort_limit_offset(sessions, limit, offset);
    }

    // Worktree-aware scanning: find all project dirs matching any worktree.
    let projects_dir = get_projects_dir(None);
    let case_insensitive = cfg!(windows);
    let fold = |s: &str| {
        if case_insensitive {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };

    // Sort worktree paths by sanitized prefix length (longest first) so more
    // specific matches take priority over shorter ones.
    let mut indexed: Vec<(String, String)> = worktree_paths
        .iter()
        .map(|wt| (wt.clone(), fold(&sanitize_path(wt))))
        .collect();
    indexed.sort_by_key(|(_, prefix)| std::cmp::Reverse(prefix.len()));

    let all_dirents: Vec<PathBuf> = match std::fs::read_dir(&projects_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => {
            // Fall back to the single project dir.
            let Some(project_dir) = find_project_dir(&canonical_dir) else {
                return apply_sort_limit_offset(Vec::new(), limit, offset);
            };
            let sessions = read_sessions_from_dir(&project_dir, Some(&canonical_dir));
            return apply_sort_limit_offset(sessions, limit, offset);
        }
    };

    let mut all_sessions = Vec::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();

    // Always include the user's actual directory (handles subdirectories
    // like /repo/packages/my-app that won't match worktree root prefixes).
    if let Some(canonical_project_dir) = find_project_dir(&canonical_dir) {
        if let Some(dir_base) = canonical_project_dir.file_name().and_then(|n| n.to_str()) {
            seen_dirs.insert(fold(dir_base));
        }
        all_sessions.extend(read_sessions_from_dir(
            &canonical_project_dir,
            Some(&canonical_dir),
        ));
    }

    for entry in all_dirents {
        let Some(dir_name) = entry.file_name().and_then(|n| n.to_str()).map(&fold) else {
            continue;
        };
        if seen_dirs.contains(&dir_name) {
            continue;
        }
        for (wt_path, prefix) in &indexed {
            // Only use a prefix match for truncated paths
            // (>MAX_SANITIZED_LENGTH) where a hash suffix follows. For short
            // paths, require an exact match to avoid /root/project matching
            // /root/project-foo.
            let is_match = dir_name == *prefix
                || (prefix.len() >= MAX_SANITIZED_LENGTH
                    && dir_name.starts_with(&format!("{prefix}-")));
            if is_match {
                seen_dirs.insert(dir_name.clone());
                all_sessions.extend(read_sessions_from_dir(&entry, Some(wt_path)));
                break;
            }
        }
    }

    apply_sort_limit_offset(deduplicate_by_session_id(all_sessions), limit, offset)
}

/// List sessions across all project directories.
fn list_all_sessions(limit: Option<usize>, offset: usize) -> Vec<SdkSessionInfo> {
    let projects_dir = get_projects_dir(None);
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    let mut all_sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            all_sessions.extend(read_sessions_from_dir(&path, None));
        }
    }
    apply_sort_limit_offset(deduplicate_by_session_id(all_sessions), limit, offset)
}

/// Options for [`list_sessions`].
#[derive(Debug, Clone)]
pub struct ListSessionsOptions {
    /// Directory to list sessions for. When provided, returns sessions for
    /// this project directory (and optionally its git worktrees). When
    /// `None`, returns sessions across all projects.
    pub directory: Option<String>,
    /// Maximum number of sessions to return.
    pub limit: Option<usize>,
    /// Number of sessions to skip from the start of the sorted result set.
    /// Use with `limit` for pagination.
    pub offset: usize,
    /// When `directory` is provided and the directory is inside a git
    /// repository, include sessions from all git worktree paths. Defaults to
    /// `true`.
    pub include_worktrees: bool,
}

impl Default for ListSessionsOptions {
    fn default() -> Self {
        Self {
            directory: None,
            limit: None,
            offset: 0,
            include_worktrees: true,
        }
    }
}

/// List sessions with metadata extracted from stat + head/tail reads.
///
/// When `options.directory` is provided, returns sessions for that project
/// directory and its git worktrees. When omitted, returns sessions across
/// all projects. Results are sorted by `last_modified` descending.
///
/// See [`list_sessions_from_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn list_sessions(options: ListSessionsOptions) -> Vec<SdkSessionInfo> {
    match &options.directory {
        Some(directory) if !directory.is_empty() => list_sessions_for_project(
            directory,
            options.limit,
            options.offset,
            options.include_worktrees,
        ),
        _ => list_all_sessions(options.limit, options.offset),
    }
}

// ---------------------------------------------------------------------------
// get_session_info — single-session metadata lookup
// ---------------------------------------------------------------------------

/// Read metadata for a single session by ID.
///
/// A stat + head/tail read of one file — no O(n) directory scan. Directory
/// resolution matches [`get_session_messages`]: `directory` is the project
/// path; when `None`, all project directories are searched for the session
/// file. Returns `None` if the session file is not found, is a sidechain
/// session, or has no extractable summary.
///
/// See [`get_session_info_from_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn get_session_info(session_id: &str, directory: Option<&str>) -> Option<SdkSessionInfo> {
    if !is_valid_uuid(session_id) {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if let Some(lite) = read_session_lite(&project_dir.join(&file_name)) {
                return parse_session_info_from_lite(session_id, &lite, Some(&canonical));
            }
        }
        // Worktree fallback — matches get_session_messages semantics.
        // Sessions may live under a different worktree root.
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt) {
                if let Some(lite) = read_session_lite(&wt_project_dir.join(&file_name)) {
                    return parse_session_info_from_lite(session_id, &lite, Some(&wt));
                }
            }
        }
        return None;
    }

    // No directory — search all project directories for the session file.
    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(lite) = read_session_lite(&path.join(&file_name)) {
            return parse_session_info_from_lite(session_id, &lite, None);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// get_session_messages — full transcript reconstruction
// ---------------------------------------------------------------------------

/// Transcript entry types that carry uuid + parentUuid chain links.
pub(crate) const TRANSCRIPT_ENTRY_TYPES: [&str; 5] =
    ["user", "assistant", "progress", "system", "attachment"];

/// A parsed JSONL transcript entry (loose object).
pub(crate) type TranscriptEntry = Map<String, Value>;

fn entry_str<'a>(entry: &'a TranscriptEntry, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

fn entry_bool(entry: &TranscriptEntry, key: &str) -> bool {
    entry.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn try_read_session_file(project_dir: &Path, file_name: &str) -> Option<String> {
    std::fs::read_to_string(project_dir.join(file_name))
        .ok()
        .filter(|c| !c.is_empty())
}

/// Find and read the session JSONL file.
///
/// If `directory` is provided, looks in that project directory and its git
/// worktrees (with prefix-fallback for hash mismatches on long paths).
/// Otherwise, searches all project directories.
fn read_session_file(session_id: &str, directory: Option<&str>) -> Option<String> {
    let file_name = format!("{session_id}.jsonl");

    if let Some(directory) = directory {
        let canonical_dir = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical_dir) {
            if let Some(content) = try_read_session_file(&project_dir, &file_name) {
                return Some(content);
            }
        }
        // Sessions may live under a different worktree root.
        for wt in get_worktree_paths(&canonical_dir) {
            if wt == canonical_dir {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt) {
                if let Some(content) = try_read_session_file(&wt_project_dir, &file_name) {
                    return Some(content);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        if let Some(content) = try_read_session_file(&entry.path(), &file_name) {
            return Some(content);
        }
    }
    None
}

/// Parse JSONL content into transcript entries.
///
/// Only keeps entries that have a uuid and are transcript message types
/// (user/assistant/progress/system/attachment). Skips corrupt lines.
pub(crate) fn parse_transcript_entries(content: &str) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    for line in content.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_transcript_type = entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t));
        if is_transcript_type && entry.get("uuid").is_some_and(Value::is_string) {
            entries.push(entry);
        }
    }
    entries
}

/// Build the conversation chain by finding the leaf and walking `parentUuid`.
///
/// Returns messages in chronological order (root → leaf).
///
/// Note: `logicalParentUuid` (set on compact_boundary entries) is
/// intentionally NOT followed. Post-compaction, the isCompactSummary message
/// replaces earlier messages, so following logical parents would duplicate
/// content.
pub(crate) fn build_conversation_chain<'a>(entries: &'a [TranscriptEntry]) -> Vec<TranscriptEntry> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut by_uuid: HashMap<&str, &TranscriptEntry> = HashMap::new();
    let mut entry_index: HashMap<&str, usize> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(uuid) = entry_str(entry, "uuid") {
            by_uuid.insert(uuid, entry);
            entry_index.insert(uuid, i);
        }
    }

    // Find terminal messages (no children point to them via parentUuid).
    let parent_uuids: HashSet<&str> = entries
        .iter()
        .filter_map(|e| entry_str(e, "parentUuid"))
        .filter(|p| !p.is_empty())
        .collect();
    let terminals: Vec<&TranscriptEntry> = entries
        .iter()
        .filter(|e| entry_str(e, "uuid").is_some_and(|u| !parent_uuids.contains(u)))
        .collect();

    // From each terminal, walk back to find the nearest user/assistant leaf.
    let mut leaves: Vec<&TranscriptEntry> = Vec::new();
    for terminal in terminals {
        let mut current = Some(terminal);
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(entry) = current {
            let Some(uuid) = entry_str(entry, "uuid") else {
                break;
            };
            if !seen.insert(uuid) {
                break;
            }
            if matches!(entry_str(entry, "type"), Some("user" | "assistant")) {
                leaves.push(entry);
                break;
            }
            current = entry_str(entry, "parentUuid")
                .filter(|p| !p.is_empty())
                .and_then(|p| by_uuid.get(p).copied());
        }
    }
    if leaves.is_empty() {
        return Vec::new();
    }

    // Pick the leaf from the main chain (not sidechain/team/meta),
    // preferring the highest position in the entries array (most recent in
    // file).
    let main_leaves: Vec<&TranscriptEntry> = leaves
        .iter()
        .copied()
        .filter(|leaf| {
            !entry_bool(leaf, "isSidechain")
                && entry_str(leaf, "teamName").is_none_or(str::is_empty)
                && !entry_bool(leaf, "isMeta")
        })
        .collect();

    let pick_best = |candidates: &[&'a TranscriptEntry]| -> &'a TranscriptEntry {
        let mut best = candidates[0];
        let mut best_idx = entry_str(best, "uuid")
            .and_then(|u| entry_index.get(u).copied())
            .unwrap_or(0);
        for cur in &candidates[1..] {
            let cur_idx = entry_str(cur, "uuid")
                .and_then(|u| entry_index.get(u).copied())
                .unwrap_or(0);
            if cur_idx > best_idx {
                best = cur;
                best_idx = cur_idx;
            }
        }
        best
    };

    let leaf = if main_leaves.is_empty() {
        pick_best(&leaves)
    } else {
        pick_best(&main_leaves)
    };

    // Walk from leaf to root via parentUuid.
    let mut chain: Vec<TranscriptEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        let Some(uuid) = entry_str(entry, "uuid") else {
            break;
        };
        if !seen.insert(uuid.to_string()) {
            break;
        }
        chain.push(entry.clone());
        current = entry_str(entry, "parentUuid")
            .filter(|p| !p.is_empty())
            .and_then(|p| by_uuid.get(p).copied());
    }
    chain.reverse();
    chain
}

/// Whether the entry should be included in the returned messages.
fn is_visible_message(entry: &TranscriptEntry) -> bool {
    if !matches!(entry_str(entry, "type"), Some("user" | "assistant")) {
        return false;
    }
    if entry_bool(entry, "isMeta") || entry_bool(entry, "isSidechain") {
        return false;
    }
    // Note: isCompactSummary messages are intentionally included. They
    // contain the summarized content from compacted conversations and are
    // the only representation of that content post-compaction.
    entry_str(entry, "teamName").is_none_or(str::is_empty)
}

/// Convert a transcript entry into a [`SessionMessage`].
fn to_session_message(
    entry: &TranscriptEntry,
    parent_tool_use_id: Option<&str>,
    parent_agent_id: Option<&str>,
) -> SessionMessage {
    let message_type = if entry_str(entry, "type") == Some("user") {
        SessionMessageType::User
    } else {
        SessionMessageType::Assistant
    };
    SessionMessage {
        message_type,
        uuid: entry_str(entry, "uuid").unwrap_or_default().to_string(),
        session_id: entry_str(entry, "sessionId")
            .unwrap_or_default()
            .to_string(),
        message: entry.get("message").cloned().unwrap_or(Value::Null),
        parent_tool_use_id: parent_tool_use_id.map(str::to_string),
        parent_agent_id: parent_agent_id.map(str::to_string),
    }
}

fn apply_message_paging(
    messages: Vec<SessionMessage>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if let Some(limit) = limit.filter(|l| *l > 0) {
        messages.into_iter().skip(offset).take(limit).collect()
    } else if offset > 0 {
        messages.into_iter().skip(offset).collect()
    } else {
        messages
    }
}

/// Build the conversation chain from parsed entries and apply paging. Shared
/// by the filesystem and [`SessionStore`]-backed paths.
fn entries_to_session_messages(
    entries: &[TranscriptEntry],
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let chain = build_conversation_chain(entries);
    let messages: Vec<SessionMessage> = chain
        .iter()
        .filter(|e| is_visible_message(e))
        .map(|e| to_session_message(e, None, None))
        .collect();
    apply_message_paging(messages, limit, offset)
}

/// Read a session's conversation messages from its JSONL transcript file.
///
/// Parses the full JSONL, builds the conversation chain via `parentUuid`
/// links, and returns user/assistant messages in chronological order.
/// Returns an empty list if the session is not found, the session_id is not
/// a valid UUID, or the transcript contains no visible messages.
///
/// See [`get_session_messages_from_store`] for the [`SessionStore`]-backed
/// async variant.
pub fn get_session_messages(
    session_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let Some(content) = read_session_file(session_id, directory) else {
        return Vec::new();
    };
    let entries = parse_transcript_entries(&content);
    entries_to_session_messages(&entries, limit, offset)
}

// ---------------------------------------------------------------------------
// list_subagents / get_subagent_messages — subagent transcript reading
// ---------------------------------------------------------------------------

/// Resolve the on-disk path of a session JSONL file.
///
/// Directory resolution mirrors [`read_session_file`]. Returns the path of
/// the first non-empty match, or `None` if not found.
pub(crate) fn resolve_session_file_path(
    session_id: &str,
    directory: Option<&str>,
) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");

    let stat_candidate = |project_dir: &Path| -> Option<PathBuf> {
        let candidate = project_dir.join(&file_name);
        match candidate.metadata() {
            Ok(metadata) if metadata.len() > 0 => Some(candidate),
            _ => None,
        }
    };

    if let Some(directory) = directory {
        let canonical_dir = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical_dir) {
            if let Some(found) = stat_candidate(&project_dir) {
                return Some(found);
            }
        }
        for wt in get_worktree_paths(&canonical_dir) {
            if wt == canonical_dir {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt) {
                if let Some(found) = stat_candidate(&wt_project_dir) {
                    return Some(found);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir(None);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(found) = stat_candidate(&path) {
            return Some(found);
        }
    }
    None
}

/// Resolve the subagents directory for a given session.
///
/// The session file lives at `<projectDir>/<sessionId>.jsonl` and the
/// subagents directory at `<projectDir>/<sessionId>/subagents/`.
fn resolve_subagents_dir(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    let resolved = resolve_session_file_path(session_id, directory)?;
    let session_dir = resolved.with_extension("");
    Some(session_dir.join("subagents"))
}

/// Recursively collect `agent-*.jsonl` files from a directory tree.
///
/// Subagent transcripts may live directly in `subagents/` or in nested
/// subdirectories such as `subagents/workflows/<runId>/`. Returns
/// `(agent_id, file_path)` pairs, sorted per directory.
fn collect_agent_files(base_dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(current_dir: &Path, results: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(current_dir) else {
            return;
        };
        let mut dirents: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirents.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
        for entry in dirents {
            let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if entry.is_file() && name.starts_with("agent-") && name.ends_with(".jsonl") {
                let agent_id = name["agent-".len()..name.len() - ".jsonl".len()].to_string();
                results.push((agent_id, entry));
            } else if entry.is_dir() {
                walk(&entry, results);
            }
        }
    }
    let mut results = Vec::new();
    walk(base_dir, &mut results);
    results
}

/// Build the conversation chain for a subagent transcript.
///
/// Subagent transcripts are simpler than main sessions — no compaction, no
/// sidechains, no preserved segments. Find the last user/assistant entry and
/// walk `parentUuid` links back to the root.
fn build_subagent_chain(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut by_uuid: HashMap<&str, &TranscriptEntry> = HashMap::new();
    for entry in entries {
        if let Some(uuid) = entry_str(entry, "uuid") {
            by_uuid.insert(uuid, entry);
        }
    }
    // Subagent transcripts are linear — the last user/assistant entry is the
    // leaf.
    let leaf = entries
        .iter()
        .rev()
        .find(|e| matches!(entry_str(e, "type"), Some("user" | "assistant")));
    let Some(leaf) = leaf else {
        return Vec::new();
    };

    let mut chain = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        let Some(uuid) = entry_str(entry, "uuid") else {
            break;
        };
        if !seen.insert(uuid.to_string()) {
            break;
        }
        chain.push(entry.clone());
        current = entry_str(entry, "parentUuid")
            .filter(|p| !p.is_empty())
            .and_then(|p| by_uuid.get(p).copied());
    }
    chain.reverse();
    chain
}

/// List subagent IDs for a given session by scanning the subagents
/// directory.
///
/// Subagent transcripts are stored at
/// `~/.claude/projects/<project>/<sessionId>/subagents/agent-<agentId>.jsonl`
/// (and may be nested in subdirectories such as `workflows/<runId>/`).
///
/// See [`list_subagents_from_store`] for the [`SessionStore`]-backed async
/// variant.
pub fn list_subagents(session_id: &str, directory: Option<&str>) -> Vec<String> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory) else {
        return Vec::new();
    };
    collect_agent_files(&subagents_dir)
        .into_iter()
        .map(|(agent_id, _)| agent_id)
        .collect()
}

/// `agent-<id>.jsonl` → `agent-<id>.meta.json` (same directory).
///
/// The single definition of the sidecar naming convention, shared by the
/// read path here, session import, and resume materialization.
pub(crate) fn agent_metadata_sidecar_path(transcript_path: &Path) -> PathBuf {
    let name = transcript_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let base = name.strip_suffix(".jsonl").unwrap_or(name);
    transcript_path.with_file_name(format!("{base}.meta.json"))
}

/// Read the `.meta.json` sidecar beside a subagent transcript.
///
/// Returns `None` when the sidecar is missing, unreadable, not valid JSON,
/// or not a JSON object — an unusable optional sidecar degrades to an
/// absent one.
pub(crate) fn read_agent_metadata_sidecar(transcript_path: &Path) -> Option<Map<String, Value>> {
    let content = std::fs::read_to_string(agent_metadata_sidecar_path(transcript_path)).ok()?;
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Separate the synthetic `agent_metadata` entry from transcript lines.
///
/// A subagent's [`SessionStore`] stream carries its `.meta.json` sidecar as
/// `{"type": "agent_metadata", ...}` entries alongside the transcript.
/// Returns `(metadata, transcript)` where `metadata` is the *last* such
/// entry (it is rewritten on resume, so last wins).
pub(crate) fn split_agent_metadata(
    entries: Vec<SessionStoreEntry>,
) -> (Option<Map<String, Value>>, Vec<SessionStoreEntry>) {
    let mut metadata = None;
    let mut transcript = Vec::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) == Some("agent_metadata") {
            metadata = Some(entry);
        } else {
            transcript.push(entry);
        }
    }
    (metadata, transcript)
}

/// Extract `(tool_use_id, parent_agent_id)` from an agent metadata object.
///
/// Works for both the on-disk `.meta.json` sidecar and the synthetic
/// `agent_metadata` entry a [`SessionStore`] receives in its place.
pub(crate) fn parent_ids_from_agent_metadata(
    meta: Option<&Map<String, Value>>,
) -> (Option<String>, Option<String>) {
    let Some(meta) = meta else {
        return (None, None);
    };
    (
        meta.get("toolUseId")
            .and_then(Value::as_str)
            .map(str::to_string),
        meta.get("parentAgentId")
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

/// Build the subagent chain from parsed entries and apply paging. Shared by
/// the filesystem and [`SessionStore`]-backed paths. Every message in a
/// subagent transcript shares the same parent ids.
fn entries_to_subagent_messages(
    entries: &[TranscriptEntry],
    limit: Option<usize>,
    offset: usize,
    parent_tool_use_id: Option<&str>,
    parent_agent_id: Option<&str>,
) -> Vec<SessionMessage> {
    let chain = build_subagent_chain(entries);
    let messages: Vec<SessionMessage> = chain
        .iter()
        .filter(|e| matches!(entry_str(e, "type"), Some("user" | "assistant")))
        .map(|e| to_session_message(e, parent_tool_use_id, parent_agent_id))
        .collect();
    apply_message_paging(messages, limit, offset)
}

/// Read a subagent's conversation messages from its JSONL transcript file.
///
/// Parses the subagent transcript, builds the conversation chain via
/// `parentUuid` links, and returns user/assistant messages in chronological
/// order. Each message's `parent_tool_use_id` is the id of the Agent
/// `tool_use` in the parent session that spawned this subagent (and
/// `parent_agent_id` the spawning subagent, for nested subagents), read from
/// the `agent-<agentId>.meta.json` sidecar next to the transcript; both are
/// `None` if the sidecar is missing or unusable.
///
/// See [`get_subagent_messages_from_store`] for the [`SessionStore`]-backed
/// async variant.
pub fn get_subagent_messages(
    session_id: &str,
    agent_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) || agent_id.is_empty() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory) else {
        return Vec::new();
    };

    // The agent file may be directly in subagents/ or in a nested
    // subdirectory — scan to find it.
    let matched = collect_agent_files(&subagents_dir)
        .into_iter()
        .find(|(found_id, _)| found_id == agent_id);
    let Some((_, file_path)) = matched else {
        return Vec::new();
    };

    let Ok(content) = std::fs::read_to_string(&file_path) else {
        return Vec::new();
    };
    if content.is_empty() {
        return Vec::new();
    }

    let meta = read_agent_metadata_sidecar(&file_path);
    let (parent_tool_use_id, parent_agent_id) = parent_ids_from_agent_metadata(meta.as_ref());

    let entries = parse_transcript_entries(&content);
    entries_to_subagent_messages(
        &entries,
        limit,
        offset,
        parent_tool_use_id.as_deref(),
        parent_agent_id.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// SessionStore-backed implementations
// ---------------------------------------------------------------------------

/// Derive the [`SessionStore`] `project_key` for a directory.
///
/// Defaults to the current working directory. Uses the same realpath + NFC
/// normalization + hashed sanitization the CLI uses for project directory
/// names, so keys match between local-disk transcripts and store-mirrored
/// transcripts even on filesystems that decompose Unicode.
pub fn project_key_for_directory(directory: Option<&str>) -> String {
    let abs_path = canonicalize_path(directory.unwrap_or("."));
    sanitize_path(&abs_path)
}

/// Serialize store entries to a JSONL string (one compact line each).
///
/// The [`SessionStore::load`] contract permits adapters to reorder object
/// keys (e.g. Postgres JSONB), but [`parse_session_info_from_lite`] scans
/// for `{"type":"tag"` as a line prefix. Hoist `type` to the front so the
/// store path matches the byte shape the disk path produces.
pub(crate) fn entries_to_jsonl(entries: &[SessionStoreEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        let mut reordered = Map::new();
        if let Some(type_value) = entry.get("type") {
            reordered.insert("type".to_string(), type_value.clone());
        }
        for (k, v) in entry {
            if k != "type" {
                reordered.insert(k.clone(), v.clone());
            }
        }
        out.push_str(&Value::Object(reordered).to_string());
        out.push('\n');
    }
    out
}

/// Build the head/tail/size lite shape from an in-memory JSONL string.
///
/// Matches [`read_session_lite`]'s byte semantics so the store path exposes
/// the same slice to [`parse_session_info_from_lite`] as the disk path would
/// for the same transcript.
pub(crate) fn jsonl_to_lite(jsonl: &str, mtime: i64) -> LiteSessionFile {
    let buf = jsonl.as_bytes();
    let size = buf.len() as u64;
    let head = String::from_utf8_lossy(&buf[..buf.len().min(LITE_READ_BUF_SIZE)]).to_string();
    let tail = if buf.len() > LITE_READ_BUF_SIZE {
        String::from_utf8_lossy(&buf[buf.len() - LITE_READ_BUF_SIZE..]).to_string()
    } else {
        head.clone()
    };
    LiteSessionFile {
        mtime,
        size,
        head,
        tail,
    }
}

/// Best-effort mtime: parse the last entry's `timestamp` field. Falls back
/// to the current wall-clock time when absent or unparseable.
pub(crate) fn mtime_from_jsonl_tail(jsonl: &str) -> i64 {
    let trimmed = jsonl.trim_end();
    let last_line = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(last_line) {
        if let Some(ts) = obj.get("timestamp").and_then(Value::as_str) {
            if let Some(ms) = iso_to_epoch_ms(ts) {
                return ms;
            }
        }
    }
    chrono::Utc::now().timestamp_millis()
}

/// Filter store-loaded entries to transcript message types with a `uuid`.
///
/// Mirrors [`parse_transcript_entries`] for the already-parsed object path
/// so chain-building never sees metadata-only entries (custom-title, tag,
/// agent_metadata, etc.).
fn filter_transcript_entries(entries: &[SessionStoreEntry]) -> Vec<TranscriptEntry> {
    entries
        .iter()
        .filter(|e| {
            e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t))
                && e.get("uuid").is_some_and(Value::is_string)
        })
        .cloned()
        .collect()
}

/// Load entries from a [`SessionStore`] and serialize to a JSONL string.
/// Returns `None` if the session has no entries.
async fn load_store_entries_as_jsonl(
    store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Option<String>> {
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    let entries = store.load(&key).await?;
    Ok(entries
        .filter(|e| !e.is_empty())
        .map(|e| entries_to_jsonl(&e)))
}

/// Derive [`SdkSessionInfo`] for each listing entry via per-session
/// `store.load()` + lite-parse.
///
/// Loads run concurrently with a fixed bound so large listings don't exhaust
/// adapter connection pools or hit backend rate limits; adapter errors
/// degrade that row to an empty summary instead of failing the whole list.
/// Sidechain and no-summary sessions are dropped.
async fn derive_infos_via_load(
    session_store: &Arc<dyn SessionStore>,
    listing: &[(String, i64)],
    directory: Option<&str>,
    project_path: &str,
) -> Vec<SdkSessionInfo> {
    let settled: Vec<Result<Option<String>>> = futures::stream::iter(listing.iter())
        .map(|(sid, _)| {
            let store = session_store.clone();
            let sid = sid.clone();
            let directory = directory.map(str::to_string);
            async move { load_store_entries_as_jsonl(&store, &sid, directory.as_deref()).await }
        })
        .buffered(STORE_LIST_LOAD_CONCURRENCY)
        .collect()
        .await;

    let mut results = Vec::new();
    for ((sid, mtime), outcome) in listing.iter().zip(settled) {
        match outcome {
            Err(_) => {
                results.push(SdkSessionInfo {
                    session_id: sid.clone(),
                    summary: String::new(),
                    last_modified: *mtime,
                    file_size: None,
                    custom_title: None,
                    first_prompt: None,
                    git_branch: None,
                    cwd: None,
                    tag: None,
                    created_at: None,
                });
            }
            Ok(None) => {}
            Ok(Some(jsonl)) => {
                if let Some(mut parsed) = parse_session_info_from_lite(
                    sid,
                    &jsonl_to_lite(&jsonl, *mtime),
                    Some(project_path),
                ) {
                    parsed.last_modified = *mtime;
                    results.push(parsed);
                }
                // Sidechain or no extractable summary — drop, matching the
                // filesystem path.
            }
        }
    }
    results
}

/// List sessions from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`list_sessions`]. Loads each
/// session's entries to derive a real summary via the same lite-parse used
/// by the filesystem path, so disk and store paths produce identical results
/// for the same transcript content.
///
/// If the store implements [`SessionStore::list_session_summaries`], this is
/// one batch summary call plus one cheap [`SessionStore::list_sessions`]
/// enumeration to gap-fill sessions missing a sidecar or whose sidecar is
/// stale — zero per-session `load()` calls when sidecars are complete and
/// fresh. Otherwise falls back to one `load()` per session (bounded at 16
/// concurrent).
///
/// Note: worktree scanning is a filesystem concept and is not honored on the
/// store path — the store operates on a single `project_key`.
///
/// # Errors
///
/// Returns [`ClaudeSdkError::InvalidConfig`] if `session_store` implements
/// neither `list_session_summaries` nor `list_sessions`.
pub async fn list_sessions_from_store(
    session_store: &Arc<dyn SessionStore>,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SdkSessionInfo>> {
    let project_path = canonicalize_path(directory.unwrap_or("."));
    let project_key = sanitize_path(&project_path);
    let has_list_sessions = session_store.implements(SessionStoreMethod::ListSessions);

    // Fast path: if the store maintains incremental summaries, fetch them in
    // one call instead of N per-session load()s.
    if session_store.implements(SessionStoreMethod::ListSessionSummaries) {
        let summaries_result = session_store.list_session_summaries(&project_key).await;
        match summaries_result {
            Err(ClaudeSdkError::StoreUnimplemented { .. }) => {}
            Err(e) => return Err(e),
            Ok(summaries) => {
                // Build a unified slot list. Fresh summaries (mtime >= the
                // session's current mtime from list_sessions) get their info
                // up front; sessions present in list_sessions() but missing
                // OR with a stale sidecar get a placeholder slot routed
                // through the same gap-fill path so the fold is recomputed
                // from source entries. Summary-backed sidechain/empty
                // sessions are dropped here (free — already determined) so
                // they don't consume offset/limit positions.
                let (listing, known_mtimes) = if has_list_sessions {
                    let listing = session_store.list_sessions(&project_key).await?;
                    let known: HashMap<String, i64> = listing
                        .iter()
                        .map(|e| (e.session_id.clone(), e.mtime))
                        .collect();
                    (listing, known)
                } else {
                    tracing::debug!(
                        target: "claude_agent_sdk",
                        "list_session_summaries without list_sessions: gap-fill skipped; \
                         sessions lacking a sidecar will be omitted"
                    );
                    (Vec::new(), HashMap::new())
                };

                struct Slot {
                    mtime: i64,
                    session_id: String,
                    info: Option<SdkSessionInfo>,
                    needs_fill: bool,
                }
                let mut slots: Vec<Slot> = Vec::new();
                let mut fresh_summary_ids: HashSet<String> = HashSet::new();
                for s in &summaries {
                    if has_list_sessions {
                        let Some(&known) = known_mtimes.get(&s.session_id) else {
                            // Summary for a session list_sessions() no longer
                            // reports — drop it.
                            continue;
                        };
                        if s.mtime < known {
                            // Stale sidecar — let gap-fill re-fold from
                            // source.
                            continue;
                        }
                    }
                    match crate::session_summary::summary_entry_to_sdk_info(s, Some(&project_path))
                    {
                        Some(info) => {
                            slots.push(Slot {
                                mtime: s.mtime,
                                session_id: s.session_id.clone(),
                                info: Some(info),
                                needs_fill: false,
                            });
                            fresh_summary_ids.insert(s.session_id.clone());
                        }
                        None => {
                            fresh_summary_ids.insert(s.session_id.clone());
                        }
                    }
                }
                if has_list_sessions {
                    for e in &listing {
                        if !fresh_summary_ids.contains(&e.session_id) {
                            slots.push(Slot {
                                mtime: e.mtime,
                                session_id: e.session_id.clone(),
                                info: None,
                                needs_fill: true,
                            });
                        }
                    }
                }

                // Paginate BEFORE per-session load so the gap-fill load()
                // count is bounded by page size, not total missing.
                slots.sort_by_key(|sl| std::cmp::Reverse(sl.mtime));
                let mut page: Vec<Slot> = if offset > 0 {
                    slots.into_iter().skip(offset).collect()
                } else {
                    slots
                };
                if let Some(limit) = limit.filter(|l| *l > 0) {
                    page.truncate(limit);
                }

                let to_fill: Vec<(String, i64)> = page
                    .iter()
                    .filter(|sl| sl.needs_fill)
                    .map(|sl| (sl.session_id.clone(), sl.mtime))
                    .collect();
                if !to_fill.is_empty() {
                    let filled =
                        derive_infos_via_load(session_store, &to_fill, directory, &project_path)
                            .await;
                    let by_sid: HashMap<String, SdkSessionInfo> = filled
                        .into_iter()
                        .map(|f| (f.session_id.clone(), f))
                        .collect();
                    for sl in page.iter_mut().filter(|sl| sl.needs_fill) {
                        sl.info = by_sid.get(&sl.session_id).cloned();
                    }
                }

                // Gap-fill placeholders that resolved to None (sidechain / no
                // extractable summary after load) are dropped here, AFTER
                // pagination — that case alone can short-page.
                return Ok(page.into_iter().filter_map(|sl| sl.info).collect());
            }
        }
    }

    if !has_list_sessions {
        return Err(ClaudeSdkError::InvalidConfig(
            "session_store implements neither list_session_summaries() nor list_sessions() -- \
             cannot list sessions. Provide a store with at least one of those methods."
                .to_string(),
        ));
    }
    let listing: Vec<(String, i64)> = session_store
        .list_sessions(&project_key)
        .await?
        .into_iter()
        .map(|e| (e.session_id, e.mtime))
        .collect();
    // Derive a real summary per session by loading its entries and reusing
    // the filesystem path's lite-parse. Filtering (sidechain/empty drop)
    // happens before pagination so limit/offset index the same filtered set
    // as the disk path.
    let results = derive_infos_via_load(session_store, &listing, directory, &project_path).await;
    Ok(apply_sort_limit_offset(results, limit, offset))
}

/// Read metadata for a single session from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`get_session_info`]. Returns `None`
/// if the session is not found, the `session_id` is not a valid UUID, the
/// session is a sidechain session, or it has no extractable summary.
pub async fn get_session_info_from_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Option<SdkSessionInfo>> {
    if !is_valid_uuid(session_id) {
        return Ok(None);
    }
    let Some(jsonl) = load_store_entries_as_jsonl(session_store, session_id, directory).await?
    else {
        return Ok(None);
    };
    let lite = jsonl_to_lite(&jsonl, mtime_from_jsonl_tail(&jsonl));
    let project_path = canonicalize_path(directory.unwrap_or("."));
    Ok(parse_session_info_from_lite(
        session_id,
        &lite,
        Some(&project_path),
    ))
}

/// Read a session's conversation messages from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`get_session_messages`]. Feeds
/// [`SessionStore::load`] results directly into the chain builder — no JSONL
/// round-trip.
pub async fn get_session_messages_from_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if !is_valid_uuid(session_id) {
        return Ok(Vec::new());
    }
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    let Some(entries) = session_store.load(&key).await? else {
        return Ok(Vec::new());
    };
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    Ok(entries_to_session_messages(
        &filter_transcript_entries(&entries),
        limit,
        offset,
    ))
}

/// List subagent IDs for a session from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`list_subagents`].
///
/// # Errors
///
/// Returns [`ClaudeSdkError::InvalidConfig`] if `session_store` does not
/// implement [`SessionStore::list_subkeys`].
pub async fn list_subagents_from_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Vec<String>> {
    if !is_valid_uuid(session_id) {
        return Ok(Vec::new());
    }
    if !session_store.implements(SessionStoreMethod::ListSubkeys) {
        return Err(ClaudeSdkError::InvalidConfig(
            "session_store does not implement list_subkeys() -- cannot list subagents. Provide a \
             store with a list_subkeys() method."
                .to_string(),
        ));
    }
    let subkeys = session_store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: project_key_for_directory(directory),
            session_id: session_id.to_string(),
        })
        .await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut ids = Vec::new();
    for subpath in subkeys {
        if !subpath.starts_with("subagents/") {
            continue;
        }
        let last = subpath.rsplit('/').next().unwrap_or("");
        if let Some(agent_id) = last.strip_prefix("agent-") {
            if seen.insert(agent_id.to_string()) {
                ids.push(agent_id.to_string());
            }
        }
    }
    Ok(ids)
}

/// Read a subagent's conversation messages from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`get_subagent_messages`]. Subagents
/// may live at `subagents/agent-<id>` or nested under
/// `subagents/workflows/<runId>/agent-<id>`. Scans subkeys when the store
/// implements [`SessionStore::list_subkeys`]; otherwise tries the direct
/// path. `parent_tool_use_id` / `parent_agent_id` are taken from the
/// subagent's `agent_metadata` entry in the store (`None` if absent).
pub async fn get_subagent_messages_from_store(
    session_store: &Arc<dyn SessionStore>,
    session_id: &str,
    agent_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if !is_valid_uuid(session_id) || agent_id.is_empty() {
        return Ok(Vec::new());
    }
    let project_key = project_key_for_directory(directory);

    let mut subpath = format!("subagents/agent-{agent_id}");
    if session_store.implements(SessionStoreMethod::ListSubkeys) {
        let subkeys = session_store
            .list_subkeys(&SessionListSubkeysKey {
                project_key: project_key.clone(),
                session_id: session_id.to_string(),
            })
            .await?;
        let target = format!("agent-{agent_id}");
        let matched = subkeys
            .into_iter()
            .find(|sk| sk.starts_with("subagents/") && sk.rsplit('/').next() == Some(&target));
        match matched {
            Some(found) => subpath = found,
            None => return Ok(Vec::new()),
        }
    }

    let key = SessionKey::with_subpath(project_key, session_id, subpath);
    let Some(entries) = session_store.load(&key).await? else {
        return Ok(Vec::new());
    };
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // The synthetic agent_metadata entry (the store's copy of the .meta.json
    // sidecar) records which Agent tool_use spawned this subagent. Recover
    // the parent ids from it — last one wins, since the metadata is
    // rewritten on resume — then drop it: it is not a transcript line.
    let (meta_entry, transcript) = split_agent_metadata(entries);
    if transcript.is_empty() {
        return Ok(Vec::new());
    }
    let (parent_tool_use_id, parent_agent_id) = parent_ids_from_agent_metadata(meta_entry.as_ref());

    Ok(entries_to_subagent_messages(
        &filter_transcript_entries(&transcript),
        limit,
        offset,
        parent_tool_use_id.as_deref(),
        parent_agent_id.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_git(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join("git");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(format!("#!/bin/bash\n{body}\n").as_bytes())
            .unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn worktree_paths_survive_output_larger_than_pipe_buffer() {
        // Regression test: stdout is drained concurrently with the bounded
        // wait, so a listing larger than the pipe buffer (>64KB) must not
        // block git and false-trip the timeout.
        let dir = tempfile::tempdir().unwrap();
        let git = fake_git(
            dir.path(),
            r#"for i in $(seq 1 2000); do printf 'worktree /w/%04d\nHEAD abc\n\n' "$i"; done"#,
        );
        let start = std::time::Instant::now();
        let paths = worktree_paths_with_git(&git.to_string_lossy(), ".");
        assert_eq!(paths.len(), 2000);
        assert_eq!(paths[0], "/w/0001");
        assert!(
            start.elapsed() < GIT_WORKTREE_TIMEOUT,
            "large output must not hit the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_paths_time_out_on_hung_git() {
        let dir = tempfile::tempdir().unwrap();
        let git = fake_git(dir.path(), "sleep 60");
        assert!(worktree_paths_with_git(&git.to_string_lossy(), ".").is_empty());
    }

    #[test]
    fn uuid_validation() {
        assert!(is_valid_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_valid_uuid("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!is_valid_uuid("not-a-uuid"));
        assert!(!is_valid_uuid("550e8400e29b41d4a716446655440000"));
    }

    #[test]
    fn sanitize_path_replaces_and_truncates() {
        assert_eq!(
            sanitize_path("/home/user/my project"),
            "-home-user-my-project"
        );
        let long = "a".repeat(300);
        let sanitized = sanitize_path(&long);
        assert!(sanitized.len() > MAX_SANITIZED_LENGTH);
        assert!(sanitized.starts_with(&"a".repeat(MAX_SANITIZED_LENGTH)));
        assert!(sanitized.chars().nth(MAX_SANITIZED_LENGTH) == Some('-'));
    }

    #[test]
    fn simple_hash_matches_js_semantics() {
        // h = ((0<<5)-0) + 'a' = 97 → base36 "2p"
        assert_eq!(simple_hash("a"), "2p");
        assert_eq!(simple_hash(""), "0");
    }

    #[test]
    fn json_field_extraction() {
        let text = r#"{"customTitle":"first"}\n{"customTitle": "second \"quoted\""}"#;
        assert_eq!(
            extract_json_string_field(text, "customTitle").as_deref(),
            Some("first")
        );
        assert_eq!(
            extract_last_json_string_field(text, "customTitle").as_deref(),
            Some("second \"quoted\"")
        );
        assert_eq!(extract_json_string_field(text, "missing"), None);
    }

    #[test]
    fn first_prompt_extraction_skips_meta() {
        let head = concat!(
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n",
        );
        assert_eq!(extract_first_prompt_from_head(head), "real prompt");
    }

    #[test]
    fn conversation_chain_walks_parents() {
        let entries: Vec<TranscriptEntry> = [
            r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant"}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","message":{"role":"user"}}"#,
        ]
        .iter()
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(map)) => map,
            _ => panic!("bad test data"),
        })
        .collect();
        let chain = build_conversation_chain(&entries);
        let uuids: Vec<&str> = chain.iter().filter_map(|e| entry_str(e, "uuid")).collect();
        assert_eq!(uuids, vec!["u1", "a1", "u2"]);
    }
}
