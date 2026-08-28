//! Materialize a [`SessionStore`]-backed resume into a temp
//! `CLAUDE_CONFIG_DIR`.
//!
//! When `options.resume` (or `options.continue_conversation`) is paired with
//! `options.session_store`, the session JSONL almost certainly does not
//! exist on local disk — it lives in the external store. The CLI subprocess
//! only knows how to resume from a local file. This module bridges the gap:
//! it loads the session from the store, writes it to a temporary directory
//! laid out exactly like `~/.claude/`, and returns the path so the caller
//! can point the subprocess at it via `CLAUDE_CONFIG_DIR`.
//!
//! Mirrors the behavior of the TypeScript and Python SDKs.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::errors::{ClaudeSdkError, Result};
use crate::internal::transcript_mirror_batcher::{
    MirrorErrorCallback, TranscriptMirrorBatcher, MAX_PENDING_BYTES, MAX_PENDING_ENTRIES,
};
use crate::sessions::{
    agent_metadata_sidecar_path, get_projects_dir, is_valid_uuid, project_key_for_directory,
    split_agent_metadata,
};
use crate::types::{
    ClaudeAgentOptions, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
    SessionStoreFlushMode, SessionStoreMethod,
};

/// Default macOS Keychain service name for OAuth credentials when
/// `CLAUDE_CONFIG_DIR` is unset.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE_NAME: &str = "Claude Code-credentials";

/// Result of [`materialize_resume_session`].
pub(crate) struct MaterializedResume {
    /// Temporary directory laid out like `~/.claude/` — point the subprocess
    /// at it via `CLAUDE_CONFIG_DIR`.
    pub config_dir: PathBuf,
    /// Session ID to pass as `--resume`. When the input was
    /// `continue_conversation`, this is the most-recent session resolved via
    /// [`SessionStore::list_sessions`].
    pub resume_session_id: String,
}

impl MaterializedResume {
    /// Remove the temp config dir (best-effort). Call after the subprocess
    /// exits.
    pub async fn cleanup(&self) {
        rmtree_with_retry(&self.config_dir).await;
    }
}

/// Raise for invalid `session_store` option combinations.
///
/// Called before subprocess spawn so misconfiguration fails fast instead of
/// surfacing as a confusing runtime error mid-session.
pub(crate) fn validate_session_store_options(options: &ClaudeAgentOptions) -> Result<()> {
    let Some(store) = &options.session_store else {
        return Ok(());
    };

    if options.continue_conversation
        && options.resume.is_none()
        && !store.implements(SessionStoreMethod::ListSessions)
    {
        // When resume is explicitly set, list_sessions() is provably never
        // called (resume wins over continue), so a minimal store is fine.
        return Err(ClaudeSdkError::InvalidConfig(
            "continue_conversation with session_store requires the store to implement \
             list_sessions()"
                .to_string(),
        ));
    }

    if options.enable_file_checkpointing {
        return Err(ClaudeSdkError::InvalidConfig(
            "session_store cannot be combined with enable_file_checkpointing (checkpoints are \
             local-disk only and would diverge from the mirrored transcript)"
                .to_string(),
        ));
    }
    Ok(())
}

/// Return a copy of `options` repointed at a materialized temp config dir.
///
/// Sets `CLAUDE_CONFIG_DIR` in `env`, `resume` to the materialized session
/// id, and clears `continue_conversation` (already resolved to a concrete
/// session id during materialization).
pub(crate) fn apply_materialized_options(
    options: ClaudeAgentOptions,
    materialized: &MaterializedResume,
) -> ClaudeAgentOptions {
    let mut env = options.env.clone();
    env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        materialized.config_dir.to_string_lossy().to_string(),
    );
    ClaudeAgentOptions {
        env,
        resume: Some(materialized.resume_session_id.clone()),
        continue_conversation: false,
        ..options
    }
}

/// Construct the [`TranscriptMirrorBatcher`] for a session.
///
/// Resolves `projects_dir` to the materialized temp dir when present (so
/// file_path → key resolution matches what the subprocess writes), otherwise
/// to the standard projects directory under the effective
/// `CLAUDE_CONFIG_DIR`.
///
/// [`SessionStoreFlushMode::Eager`] zeroes the batcher's pending thresholds
/// so every enqueued frame schedules a background flush; `Batched` keeps the
/// defaults (flush on `result` or 500-entry / 1 MiB overflow).
pub(crate) fn build_mirror_batcher(
    store: Arc<dyn SessionStore>,
    materialized: Option<&MaterializedResume>,
    env: &HashMap<String, String>,
    on_error: MirrorErrorCallback,
    flush_mode: SessionStoreFlushMode,
) -> Arc<TranscriptMirrorBatcher> {
    let projects_dir = match materialized {
        Some(materialized) => materialized
            .config_dir
            .join("projects")
            .to_string_lossy()
            .to_string(),
        None => get_projects_dir(Some(env)).to_string_lossy().to_string(),
    };
    let eager = flush_mode == SessionStoreFlushMode::Eager;
    TranscriptMirrorBatcher::new(
        store,
        projects_dir,
        on_error,
        if eager { 0 } else { MAX_PENDING_ENTRIES },
        if eager { 0 } else { MAX_PENDING_BYTES },
    )
}

/// Load a session from `options.session_store` and write it to a temp dir.
///
/// Returns `None` when no materialization is needed (no store, no
/// resume/continue, store has no entries, or the resolved session ID is not
/// a valid UUID) — the caller falls through to the normal (no-store)
/// resume/spawn path. For `continue_conversation` this means a fresh
/// session; for an explicit `resume` value the CLI receives it unchanged.
///
/// Fails if a store call errors or times out.
pub(crate) async fn materialize_resume_session(
    options: &ClaudeAgentOptions,
) -> Result<Option<MaterializedResume>> {
    let Some(store) = &options.session_store else {
        return Ok(None);
    };
    if options.resume.is_none() && !options.continue_conversation {
        return Ok(None);
    }

    let timeout = Duration::from_millis(options.effective_load_timeout_ms());
    let cwd = options
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let project_key = project_key_for_directory(cwd.as_deref());

    // Resolve the session ID — explicit resume wins; otherwise pick the
    // most-recently-modified non-sidechain session from the store. An empty
    // list_sessions() → fresh session (matches CLI --continue with no
    // history).
    let resolved = match &options.resume {
        Some(resume) => {
            // session_id is used as a path component below; reject anything
            // that isn't a UUID to prevent traversal and match every other
            // resume path.
            if !is_valid_uuid(resume) {
                return Ok(None);
            }
            load_candidate(store, &project_key, resume, timeout).await?
        }
        None => resolve_continue_candidate(store, &project_key, timeout).await?,
    };
    let Some((session_id, entries)) = resolved else {
        return Ok(None);
    };

    // The TempDir guard is held (not `.keep()`-ed) until materialization
    // succeeds: on an early error return — or if this future is dropped at
    // any await point (cancellation) — its Drop removes the directory, which
    // may already hold a .credentials.json copy. Mirrors the Python SDK's
    // BaseException cleanup.
    let tmp_dir = tempfile::Builder::new()
        .prefix("claude-resume-")
        .tempdir()
        .map_err(ClaudeSdkError::Io)?;
    let tmp_base = tmp_dir.path().to_path_buf();

    let project_dir = tmp_base.join("projects").join(&project_key);
    std::fs::create_dir_all(&project_dir)?;
    write_jsonl(&project_dir.join(format!("{session_id}.jsonl")), &entries)?;

    // The subprocess will run with CLAUDE_CONFIG_DIR=tmp_base. Copy auth
    // config from the caller's effective config locations so it can
    // authenticate. Missing files are fine (API-key auth, etc.).
    copy_auth_files(&tmp_base, &options.env);

    // Materialize subagent transcripts if the store can enumerate them.
    if store.implements(SessionStoreMethod::ListSubkeys) {
        materialize_subkeys(store, &project_dir, &project_key, &session_id, timeout).await?;
    }

    // Success: detach from the guard; cleanup is now explicit via
    // MaterializedResume::cleanup() after the subprocess exits.
    Ok(Some(MaterializedResume {
        config_dir: tmp_dir.keep(),
        resume_session_id: session_id,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort recursive removal with retries on transient lock errors.
///
/// On Windows, AV/indexer can briefly hold a handle on freshly-written files
/// (notably `.credentials.json`), causing removal to fail. Retry a few times
/// with a short backoff; after exhausting retries, ignore errors (but the
/// handle got a chance to release first so the access token doesn't leak in
/// temp). Never fails.
async fn rmtree_with_retry(path: &Path) {
    if !path.exists() {
        return;
    }
    for _ in 0..4 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ResourceBusy
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(_) => break,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(path);
}

/// Await a store call with a timeout, mapping failures to a contextual
/// error.
async fn with_timeout<T, F>(fut: F, timeout: Duration, what: &str) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(ClaudeSdkError::other(format!(
            "{what} failed during resume materialization: {e}"
        ))),
        Err(_) => Err(ClaudeSdkError::other(format!(
            "{what} timed out after {}ms during resume materialization",
            timeout.as_millis()
        ))),
    }
}

/// Load entries for `session_id`; `None` if empty/missing.
async fn load_candidate(
    store: &Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
    timeout: Duration,
) -> Result<Option<(String, Vec<SessionStoreEntry>)>> {
    let key = SessionKey::new(project_key, session_id);
    let entries = with_timeout(
        store.load(&key),
        timeout,
        &format!("SessionStore.load() for session {session_id}"),
    )
    .await?;
    Ok(entries
        .filter(|e| !e.is_empty())
        .map(|e| (session_id.to_string(), e)))
}

/// Pick the most-recently-modified non-sidechain session.
///
/// Sidechain transcripts are mirrored as ordinary top-level keys and often
/// have the highest mtime (their append lands after the main session's in
/// the same flush). Walk newest→oldest, loading each candidate (the load is
/// needed anyway) and skipping sidechains so `--continue` resumes the user's
/// conversation, not a subagent's. Matches the CLI's own `--continue`
/// filter.
async fn resolve_continue_candidate(
    store: &Arc<dyn SessionStore>,
    project_key: &str,
    timeout: Duration,
) -> Result<Option<(String, Vec<SessionStoreEntry>)>> {
    let mut sessions = with_timeout(
        store.list_sessions(project_key),
        timeout,
        "SessionStore.list_sessions()",
    )
    .await?;
    if sessions.is_empty() {
        return Ok(None);
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    for cand in sessions {
        if !is_valid_uuid(&cand.session_id) {
            continue;
        }
        let Some(loaded) = load_candidate(store, project_key, &cand.session_id, timeout).await?
        else {
            continue;
        };
        let is_sidechain = loaded
            .1
            .first()
            .and_then(|first| first.get("isSidechain"))
            .and_then(Value::as_bool)
            == Some(true);
        if is_sidechain {
            continue;
        }
        return Ok(Some(loaded));
    }
    Ok(None)
}

/// Stream-write entries as one JSON line each to `path` (mode 0o600).
fn write_jsonl(path: &Path, entries: &[SessionStoreEntry]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    for entry in entries {
        file.write_all(Value::Object(entry.clone()).to_string().as_bytes())?;
        file.write_all(b"\n")?;
    }
    restrict_permissions(path);
    Ok(())
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Seed `tmp_base` with the caller's auth and user config:
/// `.credentials.json` (refreshToken redacted), `.claude.json`, and user
/// `settings.json` / `cowork_settings.json` (plugin declarations stripped).
///
/// Source resolution mirrors the CLI: `.credentials.json`, `settings.json`
/// and `cowork_settings.json` live under the config dir (default
/// `~/.claude/`); `.claude.json` lives at `$CLAUDE_CONFIG_DIR/.claude.json`
/// when set, else `~/.claude.json` (NOT `~/.claude/.claude.json`).
fn copy_auth_files(tmp_base: &Path, opt_env: &HashMap<String, String>) {
    let caller_config_dir = opt_env
        .get("CLAUDE_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
        .filter(|d| !d.is_empty());
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let source_config_dir = caller_config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));

    #[allow(unused_mut)]
    let mut creds_json: Option<String> =
        read_if_present(&source_config_dir.join(".credentials.json"))
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string());

    // macOS default setup keeps OAuth tokens in the Keychain, not a file.
    // Redirecting CLAUDE_CONFIG_DIR changes the Keychain service-name
    // suffix, so the subprocess's lookup misses and falls back to plain-text
    // storage at `${tmp_base}/.credentials.json`. Populate that file from
    // the parent's Keychain so the resumed subprocess can auth. Skipped when
    // env-based auth or a custom config dir is already in play.
    #[cfg(target_os = "macos")]
    {
        let env_auth = |key: &str| {
            opt_env
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .is_some_and(|v| !v.is_empty())
        };
        if caller_config_dir.is_none()
            && !env_auth("ANTHROPIC_API_KEY")
            && !env_auth("CLAUDE_CODE_OAUTH_TOKEN")
        {
            if let Some(keychain) = read_keychain_credentials() {
                creds_json = Some(keychain);
            }
        }
    }

    write_redacted_credentials(creds_json.as_deref(), &tmp_base.join(".credentials.json"));

    let claude_json_src = match &caller_config_dir {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => home.join(".claude.json"),
    };
    copy_if_present(&claude_json_src, &tmp_base.join(".claude.json"), None);

    // User settings carry `apiKeyHelper` (a fourth auth mechanism alongside
    // .credentials.json / Keychain / env) plus env/hooks/permissions.
    // Without it the resumed subprocess sees no user settings at all, and an
    // apiKeyHelper-only host fails with "Not logged in".
    // cowork_settings.json is the alternate filename the CLI reads in
    // cowork-plugins mode. Both pass through the resume strip so plugin
    // declarations don't reconcile against the empty tmp_base plugin cache.
    for name in ["settings.json", "cowork_settings.json"] {
        copy_if_present(
            &source_config_dir.join(name),
            &tmp_base.join(name),
            Some(&strip_settings_for_resume),
        );
    }
}

/// User-settings keys that only misbehave under the redirected
/// `CLAUDE_CONFIG_DIR`: plugin declarations reconcile against the
/// always-empty tmp_base/plugins cache and would network-install each
/// declared marketplace on every resume.
const RESUME_SETTINGS_STRIPPED_KEYS: [&str; 2] = ["enabledPlugins", "extraKnownMarketplaces"];

/// Drop settings keys that misbehave under a redirected config dir.
///
/// Removes [`RESUME_SETTINGS_STRIPPED_KEYS`] and `env.CLAUDE_CONFIG_DIR`
/// (which would point the subprocess's config reads away from `tmp_base`).
/// Content that doesn't parse as a JSON object is returned untouched so the
/// subprocess sees exactly what the CLI would have read.
fn strip_settings_for_resume(content: Vec<u8>) -> Vec<u8> {
    // Mirror the CLI's settings reader: PowerShell writes settings.json with
    // a UTF-8 BOM, which a strict parser rejects.
    let text = String::from_utf8_lossy(&content);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Ok(Value::Object(mut parsed)) = serde_json::from_str::<Value>(text) else {
        return content;
    };
    let mut stripped = false;
    for key in RESUME_SETTINGS_STRIPPED_KEYS {
        if parsed.remove(key).is_some() {
            stripped = true;
        }
    }
    if let Some(Value::Object(env_block)) = parsed.get_mut("env") {
        if env_block.remove("CLAUDE_CONFIG_DIR").is_some() {
            stripped = true;
        }
    }
    if !stripped {
        return content;
    }
    Value::Object(parsed).to_string().into_bytes()
}

/// Write `creds_json` with `claudeAiOauth.refreshToken` removed.
///
/// The resumed subprocess runs under a redirected `CLAUDE_CONFIG_DIR`; if it
/// refreshed, the single-use refresh token would be consumed server-side and
/// the new tokens written to a location the parent never reads back —
/// leaving the parent's stored creds revoked. With no `refreshToken`, the
/// subprocess's refresh check short-circuits.
fn write_redacted_credentials(creds_json: Option<&str>, dst: &Path) {
    let Some(creds_json) = creds_json else {
        return;
    };
    let out = match serde_json::from_str::<Value>(creds_json) {
        Ok(Value::Object(mut data)) => {
            if let Some(Value::Object(oauth)) = data.get_mut("claudeAiOauth") {
                oauth.remove("refreshToken");
            }
            Value::Object(data).to_string()
        }
        // Unparseable — write through; the subprocess will fail to parse it
        // too.
        _ => creds_json.to_string(),
    };
    if std::fs::write(dst, out).is_ok() {
        restrict_permissions(dst);
    }
}

/// Read a regular file, or return `None`.
///
/// A missing source is skipped silently. Any other reason it can't be read
/// (permissions, a directory where a file was expected, ...) is logged and
/// skipped: these files are best-effort enrichment of the temp config dir,
/// so an unreadable one must not abort the resume.
fn read_if_present(src: &Path) -> Option<Vec<u8>> {
    match src.metadata() {
        Ok(metadata) if metadata.is_file() => match std::fs::read(src) {
            Ok(content) => Some(content),
            Err(e) => {
                tracing::warn!(
                    target: "claude_agent_sdk",
                    "[SessionStore] resume: skipping {} ({e})",
                    src.display()
                );
                None
            }
        },
        Ok(_) => {
            tracing::warn!(
                target: "claude_agent_sdk",
                "[SessionStore] resume: skipping {} (not a regular file)",
                src.display()
            );
            None
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                target: "claude_agent_sdk",
                "[SessionStore] resume: skipping {} ({e})",
                src.display()
            );
            None
        }
    }
}

/// Copy `src` to `dst` (mode 0o600) if it exists, through an optional
/// transform. See [`read_if_present`] for the skip policy.
fn copy_if_present(src: &Path, dst: &Path, transform: Option<&dyn Fn(Vec<u8>) -> Vec<u8>>) {
    let Some(content) = read_if_present(src) else {
        return;
    };
    let content = match transform {
        Some(transform) => transform(content),
        None => content,
    };
    match std::fs::write(dst, content) {
        Ok(()) => restrict_permissions(dst),
        Err(e) => {
            // Don't leave a truncated dst behind for the subprocess to
            // misparse.
            let _ = std::fs::remove_file(dst);
            tracing::warn!(
                target: "claude_agent_sdk",
                "[SessionStore] resume: skipping {} ({e})",
                src.display()
            );
        }
    }
}

/// Read OAuth credentials JSON from the macOS Keychain (default service
/// name). Best-effort — returns `None` on any error.
#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<String> {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &user,
            "-w",
            "-s",
            KEYCHAIN_SERVICE_NAME,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Load and write all subagent transcripts/metadata under `session_id`.
async fn materialize_subkeys(
    store: &Arc<dyn SessionStore>,
    project_dir: &Path,
    project_key: &str,
    session_id: &str,
    timeout: Duration,
) -> Result<()> {
    let session_dir = project_dir.join(session_id);
    let subkeys = with_timeout(
        store.list_subkeys(&SessionListSubkeysKey {
            project_key: project_key.to_string(),
            session_id: session_id.to_string(),
        }),
        timeout,
        &format!("SessionStore.list_subkeys() for session {session_id}"),
    )
    .await?;

    for subpath in subkeys {
        // Subpaths come from an external store and are used as filesystem
        // path components below. Reject anything that would escape the
        // session directory.
        if !is_safe_subpath(&subpath, &session_dir) {
            tracing::warn!(
                target: "claude_agent_sdk",
                "[SessionStore] skipping unsafe subpath from list_subkeys: {subpath:?}"
            );
            continue;
        }

        let sub_key = SessionKey::with_subpath(project_key, session_id, subpath.clone());
        let sub_entries = with_timeout(
            store.load(&sub_key),
            timeout,
            &format!("SessionStore.load() for session {session_id} subpath {subpath}"),
        )
        .await?;
        let Some(sub_entries) = sub_entries.filter(|e| !e.is_empty()) else {
            continue;
        };

        // agent_metadata entries describe the .meta.json sidecar (last one
        // wins); everything else is a transcript line.
        let (metadata, transcript) = split_agent_metadata(sub_entries);

        let target = session_dir.join(&subpath);
        let sub_file = target.with_file_name(format!(
            "{}.jsonl",
            target.file_name().and_then(|n| n.to_str()).unwrap_or("")
        ));
        if !transcript.is_empty() {
            write_jsonl(&sub_file, &transcript)?;
        }

        if let Some(metadata) = metadata {
            // Strip the synthetic `type` field.
            let meta_content: Map<String, Value> =
                metadata.into_iter().filter(|(k, _)| k != "type").collect();
            let meta_file = agent_metadata_sidecar_path(&sub_file);
            if let Some(parent) = meta_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&meta_file, Value::Object(meta_content).to_string())?;
            restrict_permissions(&meta_file);
        }
    }
    Ok(())
}

/// Reject subpaths that are empty, absolute, contain `.`/`..` components,
/// carry drive/UNC prefixes or NULs, or escape `session_dir` after
/// resolution.
fn is_safe_subpath(subpath: &str, session_dir: &Path) -> bool {
    if subpath.is_empty() {
        return false;
    }
    // Subpaths are store keys that may use either separator regardless of
    // host OS.
    if subpath.starts_with('/') || subpath.starts_with('\\') {
        return false;
    }
    // Drive-prefixed (`C:foo`) and UNC subpaths are never legitimate store
    // keys; rejecting ':' anywhere covers both on every host, and the only
    // subpaths ever emitted are `subagents/...`.
    if subpath.contains(':') || subpath.contains('\0') {
        return false;
    }
    if subpath
        .split(['/', '\\'])
        .any(|part| part == "." || part == ".." || part.is_empty())
    {
        return false;
    }
    // Confirm the .jsonl target — the same expression the writer uses so the
    // validated path can't drift from the written one — stays under
    // session_dir after lexical normalization.
    let target = session_dir.join(subpath);
    let sub_file = target.with_file_name(format!(
        "{}.jsonl",
        target.file_name().and_then(|n| n.to_str()).unwrap_or("")
    ));
    sub_file.starts_with(session_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_subpath_rejects_escapes() {
        let dir = Path::new("/tmp/session");
        assert!(is_safe_subpath("subagents/agent-a", dir));
        assert!(is_safe_subpath("subagents/workflows/run-1/agent-b", dir));
        assert!(!is_safe_subpath("", dir));
        assert!(!is_safe_subpath("/abs", dir));
        assert!(!is_safe_subpath("\\abs", dir));
        assert!(!is_safe_subpath("a/../b", dir));
        assert!(!is_safe_subpath("C:evil", dir));
        assert!(!is_safe_subpath("a//b", dir));
        assert!(!is_safe_subpath("nul\0", dir));
    }

    #[test]
    fn strip_settings_removes_plugin_keys() {
        let input = br#"{"enabledPlugins":{"a":true},"env":{"CLAUDE_CONFIG_DIR":"/x","OTHER":"y"},"keep":1}"#.to_vec();
        let output = strip_settings_for_resume(input);
        let parsed: Value = serde_json::from_slice(&output).unwrap();
        assert!(parsed.get("enabledPlugins").is_none());
        assert!(parsed["env"].get("CLAUDE_CONFIG_DIR").is_none());
        assert_eq!(parsed["env"]["OTHER"], "y");
        assert_eq!(parsed["keep"], 1);

        // Non-JSON content passes through untouched.
        let junk = b"not json".to_vec();
        assert_eq!(strip_settings_for_resume(junk.clone()), junk);
    }
}
