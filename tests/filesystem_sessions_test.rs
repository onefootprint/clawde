//! Filesystem-path tests for session listing, reading, mutations, and
//! store import, ported from the Python SDK's `test_sessions.py` /
//! `test_session_mutations.py` / `test_session_import.py`.
//!
//! All tests run inside one #[test] so the process-global
//! `CLAUDE_CONFIG_DIR` override cannot race between parallel tests.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use claude_agent_sdk::{
    delete_session, fork_session, get_session_info, get_session_messages, get_subagent_messages,
    import_session_to_store, list_sessions, list_subagents, rename_session, tag_session,
    ImportSessionOptions, InMemorySessionStore, ListSessionsOptions, SessionKey,
    SessionMessageType, SessionStore,
};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const OTHER_SESSION_ID: &str = "660e8400-e29b-41d4-a716-446655440001";
const AGENT_ID: &str = "agent123";
const U1: &str = "111e8400-e29b-41d4-a716-446655440010";
const A1: &str = "222e8400-e29b-41d4-a716-446655440011";
const U2: &str = "333e8400-e29b-41d4-a716-446655440012";

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(content.as_bytes()).unwrap();
}

fn transcript_jsonl() -> String {
    [
        format!(
            r#"{{"type":"user","uuid":"{U1}","parentUuid":null,"sessionId":"{SESSION_ID}","timestamp":"2025-02-01T10:00:00Z","cwd":"/work","gitBranch":"main","message":{{"role":"user","content":"analyze this codebase"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","uuid":"{A1}","parentUuid":"{U1}","sessionId":"{SESSION_ID}","timestamp":"2025-02-01T10:00:05Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Sure."}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","uuid":"{U2}","parentUuid":"{A1}","sessionId":"{SESSION_ID}","timestamp":"2025-02-01T10:01:00Z","message":{{"role":"user","content":"thanks"}}}}"#
        ),
    ]
    .join("\n")
        + "\n"
}

fn subagent_jsonl() -> String {
    [
        r#"{"type":"user","uuid":"s1","parentUuid":null,"sessionId":"sub","message":{"role":"user","content":"subtask"}}"#,
        r#"{"type":"assistant","uuid":"s2","parentUuid":"s1","sessionId":"sub","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
    ]
    .join("\n")
        + "\n"
}

#[test]
fn filesystem_session_workflows() {
    let config_dir = tempfile::tempdir().unwrap();
    // Everything below resolves the projects dir through CLAUDE_CONFIG_DIR.
    std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path());

    // The project directory name is the sanitized *canonicalized* cwd; use a
    // real directory so canonicalization is stable.
    let work_dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(work_dir.path()).unwrap();
    let directory = canonical.to_string_lossy().to_string();
    let project_key = claude_agent_sdk::project_key_for_directory(Some(&directory));
    let project_dir: PathBuf = config_dir.path().join("projects").join(&project_key);

    write_file(
        &project_dir.join(format!("{SESSION_ID}.jsonl")),
        &transcript_jsonl(),
    );
    // A metadata-only session is skipped by listing.
    write_file(
        &project_dir.join(format!("{OTHER_SESSION_ID}.jsonl")),
        "{\"type\":\"system\",\"uuid\":\"x\"}\n",
    );
    // Subagent transcript + metadata sidecar.
    write_file(
        &project_dir.join(format!("{SESSION_ID}/subagents/agent-{AGENT_ID}.jsonl")),
        &subagent_jsonl(),
    );
    write_file(
        &project_dir.join(format!("{SESSION_ID}/subagents/agent-{AGENT_ID}.meta.json")),
        r#"{"toolUseId":"tu-1","parentAgentId":null,"agentType":"general-purpose"}"#,
    );

    // --- list_sessions -------------------------------------------------
    let sessions = list_sessions(ListSessionsOptions {
        directory: Some(directory.clone()),
        include_worktrees: false,
        ..Default::default()
    });
    assert_eq!(sessions.len(), 1);
    let info = &sessions[0];
    assert_eq!(info.session_id, SESSION_ID);
    assert_eq!(info.summary, "analyze this codebase");
    assert_eq!(info.first_prompt.as_deref(), Some("analyze this codebase"));
    assert_eq!(info.git_branch.as_deref(), Some("main"));
    assert_eq!(info.cwd.as_deref(), Some("/work"));
    assert!(info.file_size.is_some());
    // created_at comes from the first entry's ISO timestamp.
    assert_eq!(info.created_at, Some(1_738_404_000_000));

    // Pagination.
    assert!(list_sessions(ListSessionsOptions {
        directory: Some(directory.clone()),
        offset: 1,
        include_worktrees: false,
        ..Default::default()
    })
    .is_empty());

    // --- get_session_info ---------------------------------------------
    let info = get_session_info(SESSION_ID, Some(&directory)).expect("info");
    assert_eq!(info.summary, "analyze this codebase");
    assert!(get_session_info("not-a-uuid", Some(&directory)).is_none());
    assert!(get_session_info(OTHER_SESSION_ID, Some(&directory)).is_none());

    // --- get_session_messages -----------------------------------------
    let messages = get_session_messages(SESSION_ID, Some(&directory), None, 0);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].uuid, U1);
    assert_eq!(messages[0].message_type, SessionMessageType::User);
    assert_eq!(messages[1].message_type, SessionMessageType::Assistant);
    let paged = get_session_messages(SESSION_ID, Some(&directory), Some(1), 1);
    assert_eq!(paged.len(), 1);
    assert_eq!(paged[0].uuid, A1);

    // --- subagents ------------------------------------------------------
    assert_eq!(
        list_subagents(SESSION_ID, Some(&directory)),
        vec![AGENT_ID.to_string()]
    );
    let sub_messages = get_subagent_messages(SESSION_ID, AGENT_ID, Some(&directory), None, 0);
    assert_eq!(sub_messages.len(), 2);
    // Parent ids come from the .meta.json sidecar.
    assert_eq!(sub_messages[0].parent_tool_use_id.as_deref(), Some("tu-1"));
    assert!(sub_messages[0].parent_agent_id.is_none());

    // --- rename / tag ----------------------------------------------------
    rename_session(SESSION_ID, "  My Analysis  ", Some(&directory)).unwrap();
    tag_session(SESSION_ID, Some("research"), Some(&directory)).unwrap();
    let info = get_session_info(SESSION_ID, Some(&directory)).unwrap();
    assert_eq!(info.custom_title.as_deref(), Some("My Analysis"));
    assert_eq!(info.summary, "My Analysis");
    assert_eq!(info.tag.as_deref(), Some("research"));

    // Clearing the tag appends an empty tag entry.
    tag_session(SESSION_ID, None, Some(&directory)).unwrap();
    let info = get_session_info(SESSION_ID, Some(&directory)).unwrap();
    assert!(info.tag.is_none());

    // Validation errors.
    assert!(rename_session("bad-id", "title", Some(&directory)).is_err());
    assert!(rename_session(SESSION_ID, "   ", Some(&directory)).is_err());
    // Renaming a missing session fails with SessionNotFound.
    let missing = "770e8400-e29b-41d4-a716-446655440002";
    assert!(matches!(
        rename_session(missing, "x", Some(&directory)),
        Err(claude_agent_sdk::ClaudeSdkError::SessionNotFound(_))
    ));

    // --- fork -----------------------------------------------------------
    let fork = fork_session(SESSION_ID, Some(&directory), Some(A1), None).unwrap();
    let fork_messages = get_session_messages(&fork.session_id, Some(&directory), None, 0);
    // Truncated at a1 (inclusive) → u1 + a1, with fresh uuids and the chain
    // preserved.
    assert_eq!(fork_messages.len(), 2);
    assert_ne!(fork_messages[0].uuid, U1);
    let fork_info = get_session_info(&fork.session_id, Some(&directory)).unwrap();
    assert_eq!(
        fork_info.custom_title.as_deref(),
        Some("My Analysis (fork)")
    );

    // --- import to store -------------------------------------------------
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        import_session_to_store(
            SESSION_ID,
            &store,
            ImportSessionOptions {
                directory: Some(directory.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let main = store
            .load(&SessionKey::new(project_key.clone(), SESSION_ID))
            .await
            .unwrap()
            .expect("imported main transcript");
        // 3 transcript entries + custom-title + 2 tag entries appended above.
        assert_eq!(main.len(), 6);

        let sub = store
            .load(&SessionKey::with_subpath(
                project_key.clone(),
                SESSION_ID,
                format!("subagents/agent-{AGENT_ID}"),
            ))
            .await
            .unwrap()
            .expect("imported subagent transcript");
        // 2 transcript entries + the synthetic agent_metadata entry.
        assert_eq!(sub.len(), 3);
        assert!(sub
            .iter()
            .any(|e| e.get("type") == Some(&serde_json::json!("agent_metadata"))));
    });

    // --- delete ----------------------------------------------------------
    delete_session(SESSION_ID, Some(&directory)).unwrap();
    assert!(get_session_info(SESSION_ID, Some(&directory)).is_none());
    // The subagent directory is removed too.
    assert!(!project_dir.join(SESSION_ID).exists());
    // Double delete fails.
    assert!(delete_session(SESSION_ID, Some(&directory)).is_err());

    std::env::remove_var("CLAUDE_CONFIG_DIR");
}
