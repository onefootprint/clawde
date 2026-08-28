//! Wire-format serialization tests for the SDK's typed values, ported from
//! the Python SDK's `test_types.py` / control-protocol expectations.

use clawde::{
    AgentDefinition, AgentEffort, AsyncHookJsonOutput, ClaudeAgentOptions, EffortLevel,
    HookJsonOutput, HookSpecificOutput, PermissionBehavior, PermissionDecision, PermissionMode,
    PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination, SessionStoreFlushMode,
    SyncHookJsonOutput, ThinkingConfig, ThinkingDisplay,
};
use serde_json::json;

#[test]
fn permission_update_round_trips_wire_format() {
    let update = PermissionUpdate::AddRules {
        rules: Some(vec![PermissionRuleValue {
            tool_name: "Bash".to_string(),
            rule_content: Some("ls:*".to_string()),
        }]),
        behavior: Some(PermissionBehavior::Allow),
        destination: Some(PermissionUpdateDestination::Session),
    };
    let wire = serde_json::to_value(&update).unwrap();
    assert_eq!(
        wire,
        json!({
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "ls:*"}],
            "behavior": "allow",
            "destination": "session",
        })
    );
    let back: PermissionUpdate = serde_json::from_value(wire).unwrap();
    assert_eq!(back, update);

    let set_mode = PermissionUpdate::SetMode {
        mode: Some(PermissionMode::AcceptEdits),
        destination: None,
    };
    assert_eq!(
        serde_json::to_value(&set_mode).unwrap(),
        json!({"type": "setMode", "mode": "acceptEdits"})
    );

    // ruleContent is always present on the wire (null when unset), matching
    // the TypeScript control protocol.
    let bare_rule = PermissionUpdate::RemoveRules {
        rules: Some(vec![PermissionRuleValue {
            tool_name: "Read".to_string(),
            rule_content: None,
        }]),
        behavior: None,
        destination: None,
    };
    assert_eq!(
        serde_json::to_value(&bare_rule).unwrap(),
        json!({"type": "removeRules", "rules": [{"toolName": "Read", "ruleContent": null}]})
    );
}

#[test]
fn permission_modes_serialize_to_cli_strings() {
    for (mode, expected) in [
        (PermissionMode::Default, "default"),
        (PermissionMode::AcceptEdits, "acceptEdits"),
        (PermissionMode::Plan, "plan"),
        (PermissionMode::BypassPermissions, "bypassPermissions"),
        (PermissionMode::DontAsk, "dontAsk"),
        (PermissionMode::Auto, "auto"),
    ] {
        assert_eq!(mode.as_str(), expected);
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(expected));
    }
}

#[test]
fn hook_output_uses_cli_field_names() {
    // The Python SDK converts async_/continue_ to async/continue for the
    // CLI; the Rust types serialize straight to the wire names.
    let sync_output = HookJsonOutput::Sync(SyncHookJsonOutput {
        continue_: Some(false),
        suppress_output: Some(true),
        stop_reason: Some("blocked".to_string()),
        decision: Some("block".to_string()),
        system_message: None,
        reason: Some("nope".to_string()),
        hook_specific_output: Some(HookSpecificOutput::PreToolUse {
            permission_decision: Some(PermissionDecision::Deny),
            permission_decision_reason: Some("policy".to_string()),
            updated_input: None,
            additional_context: None,
        }),
    });
    assert_eq!(
        serde_json::to_value(&sync_output).unwrap(),
        json!({
            "continue": false,
            "suppressOutput": true,
            "stopReason": "blocked",
            "decision": "block",
            "reason": "nope",
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "policy",
            },
        })
    );

    let async_output = HookJsonOutput::Async(AsyncHookJsonOutput {
        async_: true,
        async_timeout: Some(5000),
    });
    assert_eq!(
        serde_json::to_value(&async_output).unwrap(),
        json!({"async": true, "asyncTimeout": 5000})
    );

    // Empty sync output serializes to an empty object.
    assert_eq!(
        serde_json::to_value(HookJsonOutput::default()).unwrap(),
        json!({})
    );
}

#[test]
fn agent_definition_serializes_camel_case() {
    let mut agent = AgentDefinition::new("A reviewer", "You review code.");
    agent.disallowed_tools = Some(vec!["Bash".to_string()]);
    agent.max_turns = Some(3);
    agent.effort = Some(AgentEffort::Level(EffortLevel::Low));
    agent.permission_mode = Some(PermissionMode::Plan);
    let wire = serde_json::to_value(&agent).unwrap();
    assert_eq!(
        wire,
        json!({
            "description": "A reviewer",
            "prompt": "You review code.",
            "disallowedTools": ["Bash"],
            "maxTurns": 3,
            "effort": "low",
            "permissionMode": "plan",
        })
    );

    let mut numeric = AgentDefinition::new("d", "p");
    numeric.effort = Some(AgentEffort::Value(3));
    assert_eq!(serde_json::to_value(&numeric).unwrap()["effort"], json!(3));
}

#[test]
fn thinking_config_serializes_like_python() {
    assert_eq!(
        serde_json::to_value(ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Summarized)
        })
        .unwrap(),
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(
        serde_json::to_value(ThinkingConfig::Enabled {
            budget_tokens: 1024,
            display: None
        })
        .unwrap(),
        json!({"type": "enabled", "budget_tokens": 1024})
    );
    assert_eq!(
        serde_json::to_value(ThinkingConfig::Disabled).unwrap(),
        json!({"type": "disabled"})
    );
}

#[test]
fn options_defaults_match_python() {
    let options = ClaudeAgentOptions::default();
    assert!(options.tools.is_none());
    assert!(options.allowed_tools.is_empty());
    assert!(options.system_prompt.is_none());
    assert!(options.mcp_servers.is_empty());
    assert!(!options.strict_mcp_config);
    assert!(options.permission_mode.is_none());
    assert!(!options.continue_conversation);
    assert!(options.resume.is_none());
    assert!(options.max_turns.is_none());
    assert!(options.env.is_empty());
    assert!(!options.include_partial_messages);
    assert!(!options.fork_session);
    assert_eq!(options.session_store_flush, SessionStoreFlushMode::Batched);
    assert!(options.load_timeout_ms.is_none()); // effective default: 60s
    assert!(options.task_budget.is_none());
}

#[test]
fn terminal_task_statuses_span_both_vocabularies() {
    use clawde::{is_terminal_task_status, TERMINAL_TASK_STATUSES};
    assert_eq!(
        TERMINAL_TASK_STATUSES,
        ["completed", "failed", "stopped", "killed"]
    );
    assert!(is_terminal_task_status("killed"));
    assert!(is_terminal_task_status("stopped"));
    assert!(!is_terminal_task_status("running"));
}
