//! In-memory reference implementation of [`SessionStore`].

use std::collections::HashMap;
use std::path::{Component, Path};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::errors::Result;
use crate::session_summary::fold_session_summary;
use crate::types::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry,
    SessionStoreMethod, SessionSummaryEntry,
};

fn key_to_string(key: &SessionKey) -> String {
    match &key.subpath {
        Some(subpath) if !subpath.is_empty() => {
            format!("{}/{}/{}", key.project_key, key.session_id, subpath)
        }
        _ => format!("{}/{}", key.project_key, key.session_id),
    }
}

#[derive(Default)]
struct InMemoryState {
    store: HashMap<String, Vec<SessionStoreEntry>>,
    mtimes: HashMap<String, i64>,
    summaries: HashMap<(String, String), SessionSummaryEntry>,
    last_mtime: i64,
}

impl InMemoryState {
    /// Storage write time for this adapter, in Unix epoch ms.
    ///
    /// Guaranteed strictly monotonically increasing across calls within the
    /// process so back-to-back appends always produce distinct mtimes (real
    /// storage backends get this property for free from their commit
    /// ordering).
    fn next_mtime(&mut self) -> i64 {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let stamped = now_ms.max(self.last_mtime + 1);
        self.last_mtime = stamped;
        stamped
    }
}

/// In-memory [`SessionStore`] implementation for testing and development.
///
/// Stores entries keyed by a composite `project_key/session_id` string (with
/// an optional `/subpath` suffix). Not suitable for production — data is
/// lost when the process exits.
#[derive(Default)]
pub struct InMemorySessionStore {
    state: Mutex<InMemoryState>,
}

impl InMemorySessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper — get all entries for a key (empty list if absent).
    pub async fn get_entries(&self, key: &SessionKey) -> Vec<SessionStoreEntry> {
        self.state
            .lock()
            .await
            .store
            .get(&key_to_string(key))
            .cloned()
            .unwrap_or_default()
    }

    /// Test helper — number of stored sessions (main transcripts only).
    pub async fn size(&self) -> usize {
        self.state
            .lock()
            .await
            .store
            .keys()
            .filter(|k| {
                k.find('/')
                    .is_some_and(|first_slash| !k[first_slash + 1..].contains('/'))
            })
            .count()
    }

    /// Test helper — clear all stored data.
    pub async fn clear(&self) {
        let mut state = self.state.lock().await;
        state.store.clear();
        state.mtimes.clear();
        state.summaries.clear();
        state.last_mtime = 0;
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn append(&self, key: &SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        let mut state = self.state.lock().await;
        let k = key_to_string(key);
        let now_ms = state.next_mtime();
        // Maintain the per-session summary sidecar incrementally so
        // list_session_summaries() never re-reads. Subagent subpaths don't
        // contribute to the main session's summary.
        if key.subpath.is_none() {
            let sk = (key.project_key.clone(), key.session_id.clone());
            let mut folded = fold_session_summary(state.summaries.get(&sk), key, &entries);
            // Stamp the sidecar with this adapter's storage write time — the
            // SAME clock list_sessions() exposes. SessionSummaryEntry.mtime
            // is contractually storage write time (not entry time), so the
            // fast-path staleness check works correctly.
            folded.mtime = now_ms;
            state.summaries.insert(sk, folded);
        }
        state.store.entry(k.clone()).or_default().extend(entries);
        state.mtimes.insert(k, now_ms);
        Ok(())
    }

    async fn load(&self, key: &SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        Ok(self
            .state
            .lock()
            .await
            .store
            .get(&key_to_string(key))
            .cloned())
    }

    async fn list_sessions(&self, project_key: &str) -> Result<Vec<SessionStoreListEntry>> {
        let state = self.state.lock().await;
        let prefix = format!("{project_key}/");
        let mut results = Vec::new();
        for k in state.store.keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                // Only include main transcripts (no subpath, so no second
                // '/').
                if !rest.contains('/') {
                    results.push(SessionStoreListEntry {
                        session_id: rest.to_string(),
                        mtime: state.mtimes.get(k).copied().unwrap_or(0),
                    });
                }
            }
        }
        Ok(results)
    }

    async fn list_session_summaries(&self, project_key: &str) -> Result<Vec<SessionSummaryEntry>> {
        Ok(self
            .state
            .lock()
            .await
            .summaries
            .iter()
            .filter(|((pk, _), _)| pk == project_key)
            .map(|(_, s)| s.clone())
            .collect())
    }

    async fn delete(&self, key: &SessionKey) -> Result<()> {
        let mut state = self.state.lock().await;
        let k = key_to_string(key);
        state.store.remove(&k);
        state.mtimes.remove(&k);
        // Deleting the main transcript cascades to its subkeys (subagent
        // transcripts, metadata) so they aren't orphaned. A targeted delete
        // with an explicit subpath removes only that one entry.
        if key.subpath.is_none() {
            state
                .summaries
                .remove(&(key.project_key.clone(), key.session_id.clone()));
            let prefix = format!("{}/{}/", key.project_key, key.session_id);
            let sub_keys: Vec<String> = state
                .store
                .keys()
                .filter(|sk| sk.starts_with(&prefix))
                .cloned()
                .collect();
            for sk in sub_keys {
                state.store.remove(&sk);
                state.mtimes.remove(&sk);
            }
        }
        Ok(())
    }

    async fn list_subkeys(&self, key: &SessionListSubkeysKey) -> Result<Vec<String>> {
        let state = self.state.lock().await;
        let prefix = format!("{}/{}/", key.project_key, key.session_id);
        Ok(state
            .store
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .map(str::to_string)
            .collect())
    }

    fn implements(&self, _method: SessionStoreMethod) -> bool {
        true
    }
}

/// Derive a [`SessionKey`] from an absolute transcript file path.
///
/// Main transcripts: `<projects_dir>/<project_key>/<session_id>.jsonl`.
/// Subagent transcripts:
/// `<projects_dir>/<project_key>/<session_id>/subagents/agent-<id>.jsonl`.
///
/// Returns `None` if `file_path` is not under `projects_dir` or has an
/// unrecognized shape.
pub fn file_path_to_session_key(file_path: &str, projects_dir: &str) -> Option<SessionKey> {
    let file_path = Path::new(file_path);
    let projects_dir = Path::new(projects_dir);
    let rel = file_path.strip_prefix(projects_dir).ok()?;

    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()?;
    if parts.len() < 2 {
        return None;
    }

    let project_key = parts[0].to_string();
    let second = parts[1];

    // Main transcript: <project_key>/<session_id>.jsonl
    if parts.len() == 2 {
        let session_id = second.strip_suffix(".jsonl")?;
        return Some(SessionKey::new(project_key, session_id));
    }

    // Subagent transcript:
    // <project_key>/<session_id>/subagents/.../agent-<id>.jsonl
    if parts.len() >= 4 {
        let mut subpath_parts: Vec<String> = parts[2..].iter().map(|p| p.to_string()).collect();
        if let Some(last) = subpath_parts.last_mut() {
            if let Some(stripped) = last.strip_suffix(".jsonl") {
                *last = stripped.to_string();
            }
        }
        // Subpaths are always /-joined regardless of platform separator so
        // keys are portable across platforms.
        return Some(SessionKey::with_subpath(
            project_key,
            second,
            subpath_parts.join("/"),
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_path_to_session_key_shapes() {
        let key = file_path_to_session_key("/tmp/projects/proj/abc.jsonl", "/tmp/projects")
            .expect("main transcript");
        assert_eq!(key.project_key, "proj");
        assert_eq!(key.session_id, "abc");
        assert_eq!(key.subpath, None);

        let key = file_path_to_session_key(
            "/tmp/projects/proj/abc/subagents/agent-x.jsonl",
            "/tmp/projects",
        )
        .expect("subagent transcript");
        assert_eq!(key.subpath.as_deref(), Some("subagents/agent-x"));

        let key = file_path_to_session_key(
            "/tmp/projects/proj/abc/subagents/workflows/run-1/agent-y.jsonl",
            "/tmp/projects",
        )
        .expect("nested subagent transcript");
        assert_eq!(
            key.subpath.as_deref(),
            Some("subagents/workflows/run-1/agent-y")
        );

        assert!(file_path_to_session_key("/elsewhere/proj/abc.jsonl", "/tmp/projects").is_none());
        assert!(file_path_to_session_key("/tmp/projects/proj/abc.txt", "/tmp/projects").is_none());
        // Three components (no subagents level) is unrecognized.
        assert!(
            file_path_to_session_key("/tmp/projects/proj/abc/x.jsonl", "/tmp/projects").is_none()
        );
    }
}
