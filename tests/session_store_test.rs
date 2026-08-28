//! Tests for the session store subsystem: the in-memory store, summary
//! folding, store-backed listing, and store-backed mutations. Ported from
//! the Python SDK's session-store test suite.

use std::sync::Arc;

use clawde::{
    delete_session_via_store, fold_session_summary, fork_session_via_store,
    get_session_info_from_store, get_session_messages_from_store, get_subagent_messages_from_store,
    list_sessions_from_store, list_subagents_from_store, project_key_for_directory,
    rename_session_via_store, summary_entry_to_sdk_info, tag_session_via_store,
    InMemorySessionStore, SessionKey, SessionStore, SessionStoreEntry,
};
use serde_json::{json, Map, Value};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn entry(value: Value) -> SessionStoreEntry {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn store() -> Arc<dyn SessionStore> {
    Arc::new(InMemorySessionStore::new())
}

fn transcript_entries() -> Vec<SessionStoreEntry> {
    vec![
        entry(json!({
            "type": "user", "uuid": "u1", "parentUuid": null,
            "sessionId": SESSION_ID, "timestamp": "2025-01-01T00:00:00Z",
            "cwd": "/work", "gitBranch": "main",
            "message": {"role": "user", "content": "first prompt"},
        })),
        entry(json!({
            "type": "assistant", "uuid": "a1", "parentUuid": "u1",
            "sessionId": SESSION_ID, "timestamp": "2025-01-01T00:00:01Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        })),
    ]
}

#[tokio::test]
async fn append_load_round_trip_and_listing() {
    let store = store();
    let key = SessionKey::new("proj", SESSION_ID);
    store.append(&key, transcript_entries()).await.unwrap();

    let loaded = store.load(&key).await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].get("uuid"), Some(&json!("u1")));

    // Missing keys load as None.
    let missing = SessionKey::new("proj", "660e8400-e29b-41d4-a716-446655440000");
    assert!(store.load(&missing).await.unwrap().is_none());

    let listing = store.list_sessions("proj").await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].session_id, SESSION_ID);
    assert!(listing[0].mtime > 0);

    // Subagent keys don't show up in the main listing.
    let sub_key = SessionKey::with_subpath("proj", SESSION_ID, "subagents/agent-x");
    store
        .append(&sub_key, vec![entry(json!({"type": "user", "uuid": "s1", "message": {"role": "user", "content": "sub"}}))])
        .await
        .unwrap();
    assert_eq!(store.list_sessions("proj").await.unwrap().len(), 1);
    let subkeys = store
        .list_subkeys(&clawde::SessionListSubkeysKey {
            project_key: "proj".to_string(),
            session_id: SESSION_ID.to_string(),
        })
        .await
        .unwrap();
    assert_eq!(subkeys, vec!["subagents/agent-x".to_string()]);
}

#[tokio::test]
async fn delete_cascades_to_subkeys() {
    let store = store();
    let key = SessionKey::new("proj", SESSION_ID);
    let sub_key = SessionKey::with_subpath("proj", SESSION_ID, "subagents/agent-x");
    store.append(&key, transcript_entries()).await.unwrap();
    store
        .append(&sub_key, vec![entry(json!({"type": "user", "uuid": "s1"}))])
        .await
        .unwrap();

    store.delete(&key).await.unwrap();
    assert!(store.load(&key).await.unwrap().is_none());
    assert!(store.load(&sub_key).await.unwrap().is_none());
    assert!(store.list_sessions("proj").await.unwrap().is_empty());
    assert!(store
        .list_session_summaries("proj")
        .await
        .unwrap()
        .is_empty());
}

#[test]
fn fold_session_summary_is_incremental() {
    let key = SessionKey::new("proj", SESSION_ID);
    let first_batch = transcript_entries();
    let folded = fold_session_summary(None, &key, &first_batch);
    assert_eq!(folded.session_id, SESSION_ID);
    // mtime is the adapter's to stamp.
    assert_eq!(folded.mtime, 0);

    let info = summary_entry_to_sdk_info(&folded, Some("/fallback")).expect("summary info");
    assert_eq!(info.summary, "first prompt");
    assert_eq!(info.first_prompt.as_deref(), Some("first prompt"));
    assert_eq!(info.git_branch.as_deref(), Some("main"));
    assert_eq!(info.cwd.as_deref(), Some("/work"));
    assert_eq!(info.created_at, Some(1_735_689_600_000));

    // Later batches: custom title wins, tags are last-wins, empty tag
    // clears.
    let second_batch = vec![
        entry(json!({"type": "custom-title", "customTitle": "My session"})),
        entry(json!({"type": "tag", "tag": "experiment"})),
    ];
    let folded = fold_session_summary(Some(&folded), &key, &second_batch);
    let info = summary_entry_to_sdk_info(&folded, None).unwrap();
    assert_eq!(info.summary, "My session");
    assert_eq!(info.custom_title.as_deref(), Some("My session"));
    assert_eq!(info.tag.as_deref(), Some("experiment"));

    let third_batch = vec![entry(json!({"type": "tag", "tag": ""}))];
    let folded = fold_session_summary(Some(&folded), &key, &third_batch);
    assert!(summary_entry_to_sdk_info(&folded, None)
        .unwrap()
        .tag
        .is_none());

    // Sidechain sessions produce no info.
    let sidechain = fold_session_summary(
        None,
        &key,
        &[entry(
            json!({"type": "user", "uuid": "u1", "isSidechain": true}),
        )],
    );
    assert!(summary_entry_to_sdk_info(&sidechain, None).is_none());
}

#[tokio::test]
async fn store_backed_listing_and_messages() {
    let store = store();
    let cwd = std::env::current_dir().unwrap();
    let project_key = project_key_for_directory(Some(&cwd.to_string_lossy()));
    let key = SessionKey::new(project_key, SESSION_ID);
    store.append(&key, transcript_entries()).await.unwrap();

    let sessions = list_sessions_from_store(&store, None, None, 0)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, SESSION_ID);
    assert_eq!(sessions[0].summary, "first prompt");

    let info = get_session_info_from_store(&store, SESSION_ID, None)
        .await
        .unwrap()
        .expect("session info");
    assert_eq!(info.first_prompt.as_deref(), Some("first prompt"));

    let messages = get_session_messages_from_store(&store, SESSION_ID, None, None, 0)
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].uuid, "u1");
    assert_eq!(messages[1].uuid, "a1");

    // Invalid ids return empty rather than failing.
    assert!(
        get_session_messages_from_store(&store, "not-a-uuid", None, None, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn store_backed_subagents() {
    let store = store();
    let cwd = std::env::current_dir().unwrap();
    let project_key = project_key_for_directory(Some(&cwd.to_string_lossy()));
    let sub_key = SessionKey::with_subpath(project_key, SESSION_ID, "subagents/agent-abc");
    store
        .append(
            &sub_key,
            vec![
                entry(json!({"type": "agent_metadata", "toolUseId": "tu-9", "parentAgentId": "parent-1"})),
                entry(json!({
                    "type": "user", "uuid": "su1", "parentUuid": null,
                    "sessionId": SESSION_ID,
                    "message": {"role": "user", "content": "subagent prompt"},
                })),
            ],
        )
        .await
        .unwrap();

    let agents = list_subagents_from_store(&store, SESSION_ID, None)
        .await
        .unwrap();
    assert_eq!(agents, vec!["abc".to_string()]);

    let messages = get_subagent_messages_from_store(&store, SESSION_ID, "abc", None, None, 0)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    // Parent ids come from the agent_metadata entry.
    assert_eq!(messages[0].parent_tool_use_id.as_deref(), Some("tu-9"));
    assert_eq!(messages[0].parent_agent_id.as_deref(), Some("parent-1"));
}

#[tokio::test]
async fn store_backed_mutations() {
    let store = store();
    let cwd = std::env::current_dir().unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();
    let project_key = project_key_for_directory(Some(&cwd_str));
    let key = SessionKey::new(project_key.clone(), SESSION_ID);
    store.append(&key, transcript_entries()).await.unwrap();

    rename_session_via_store(&store, SESSION_ID, "  Renamed!  ", None)
        .await
        .unwrap();
    tag_session_via_store(&store, SESSION_ID, Some("mytag"), None)
        .await
        .unwrap();

    let info = get_session_info_from_store(&store, SESSION_ID, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.custom_title.as_deref(), Some("Renamed!"));
    assert_eq!(info.tag.as_deref(), Some("mytag"));

    // Clearing the tag.
    tag_session_via_store(&store, SESSION_ID, None, None)
        .await
        .unwrap();
    let info = get_session_info_from_store(&store, SESSION_ID, None)
        .await
        .unwrap()
        .unwrap();
    assert!(info.tag.is_none());

    // Empty titles are rejected.
    assert!(rename_session_via_store(&store, SESSION_ID, "   ", None)
        .await
        .is_err());
    // Whitespace-only tags too (after sanitization).
    assert!(
        tag_session_via_store(&store, SESSION_ID, Some("\u{200b}\u{feff}"), None)
            .await
            .is_err()
    );

    // Fork: new session id, remapped uuids, forkedFrom stamped, title
    // derived with a " (fork)" suffix.
    let fork = fork_session_via_store(&store, SESSION_ID, None, None, None)
        .await
        .unwrap();
    assert_ne!(fork.session_id, SESSION_ID);
    let fork_key = SessionKey::new(project_key, fork.session_id.clone());
    let forked_entries = store.load(&fork_key).await.unwrap().unwrap();
    let first = &forked_entries[0];
    assert_eq!(first.get("sessionId"), Some(&json!(fork.session_id)));
    assert_ne!(first.get("uuid"), Some(&json!("u1")));
    assert_eq!(first["forkedFrom"]["sessionId"], json!(SESSION_ID));
    assert_eq!(first["forkedFrom"]["messageUuid"], json!("u1"));
    // The parent chain is preserved through the remap.
    let second = &forked_entries[1];
    assert_eq!(second.get("parentUuid"), first.get("uuid"));
    let title_entry = forked_entries
        .iter()
        .find(|e| e.get("type") == Some(&json!("custom-title")))
        .expect("fork title entry");
    assert_eq!(title_entry["customTitle"], json!("Renamed! (fork)"));

    // Delete via store.
    delete_session_via_store(&store, SESSION_ID, None)
        .await
        .unwrap();
    assert!(store.load(&key).await.unwrap().is_none());
}

#[tokio::test]
async fn fork_up_to_message_id() {
    let store = store();
    let cwd = std::env::current_dir().unwrap();
    let project_key = project_key_for_directory(Some(&cwd.to_string_lossy()));
    let key = SessionKey::new(project_key.clone(), SESSION_ID);

    let up_to = "770e8400-e29b-41d4-a716-446655440000";
    store
        .append(
            &key,
            vec![
                entry(json!({"type": "user", "uuid": up_to, "parentUuid": null,
                             "message": {"role": "user", "content": "keep"}})),
                entry(
                    json!({"type": "assistant", "uuid": "a2", "parentUuid": up_to,
                             "message": {"role": "assistant", "content": []}}),
                ),
            ],
        )
        .await
        .unwrap();

    let fork = fork_session_via_store(&store, SESSION_ID, None, Some(up_to), Some("Branch"))
        .await
        .unwrap();
    let fork_key = SessionKey::new(project_key, fork.session_id.clone());
    let forked = store.load(&fork_key).await.unwrap().unwrap();
    // One kept transcript entry + the custom-title entry.
    assert_eq!(forked.len(), 2);
    assert_eq!(forked[1]["customTitle"], json!("Branch"));

    // Unknown cut points are an error.
    let missing = "880e8400-e29b-41d4-a716-446655440000";
    assert!(
        fork_session_via_store(&store, SESSION_ID, None, Some(missing), None)
            .await
            .is_err()
    );
}
