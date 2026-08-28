//! A non-trivial, end-to-end example: a project task tracker driven by
//! Claude through the SDK.
//!
//! Demonstrates, in one program:
//! - an interactive multi-turn [`ClaudeSdkClient`] session;
//! - **built-in tools** (Read / Glob / Grep) so Claude can explore this
//!   crate's own source tree;
//! - an **in-process MCP server** whose tools mutate shared application
//!   state (`Arc<Mutex<Vec<Task>>>`) — no IPC, Claude's tool calls land
//!   directly in this process's memory;
//! - a **PreToolUse hook** that logs every tool call and blocks writes;
//! - a **`can_use_tool` permission callback** gating the built-in tools;
//! - streamed, typed message handling and per-turn cost reporting.
//!
//! Run with: `cargo run --example task_tracker`

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use claude_agent_sdk::mcp::{create_sdk_mcp_server, tool, CallToolResult};
use claude_agent_sdk::{
    ClaudeAgentOptions, ClaudeSdkClient, ContentBlock, HookCallback, HookEvent, HookInput,
    HookJsonOutput, HookMatcher, HookSpecificOutput, McpServerConfig, McpServers, Message,
    PermissionDecision, PermissionResult, QueryPrompt, SyncHookJsonOutput, ToolsConfig,
};
use futures::StreamExt;
use serde_json::json;

#[derive(Debug, Clone)]
struct Task {
    id: u64,
    title: String,
    priority: String,
    done: bool,
}

#[derive(Default)]
struct TaskStore {
    tasks: Mutex<Vec<Task>>,
    next_id: AtomicU64,
}

impl TaskStore {
    fn add(&self, title: String, priority: String) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.tasks.lock().unwrap().push(Task {
            id,
            title,
            priority,
            done: false,
        });
        id
    }

    fn complete(&self, id: u64) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        match tasks.iter_mut().find(|t| t.id == id) {
            Some(task) => {
                task.done = true;
                true
            }
            None => false,
        }
    }

    fn render(&self) -> String {
        let tasks = self.tasks.lock().unwrap();
        if tasks.is_empty() {
            return "(no tasks)".to_string();
        }
        tasks
            .iter()
            .map(|t| {
                format!(
                    "#{} [{}] {} ({})",
                    t.id,
                    if t.done { "x" } else { " " },
                    t.title,
                    t.priority
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Build the in-process MCP server backed by `store`. The handlers close
/// over the same `Arc` the main program reads at the end — tool calls made
/// by Claude mutate this process's state directly.
fn tracker_server(store: Arc<TaskStore>) -> claude_agent_sdk::McpSdkServerConfig {
    let add_store = store.clone();
    let add_task = tool(
        "add_task",
        "Add a task to the project task list",
        json!({
            "title": {"type": "string", "description": "Short task title"},
            "priority": {"type": "string", "enum": ["low", "medium", "high"]},
        }),
        move |args| {
            let store = add_store.clone();
            async move {
                let title = args["title"].as_str().unwrap_or("untitled").to_string();
                let priority = args["priority"].as_str().unwrap_or("medium").to_string();
                let id = store.add(title.clone(), priority);
                Ok(CallToolResult::text(format!("Created task #{id}: {title}")))
            }
        },
    );

    let complete_store = store.clone();
    let complete_task = tool(
        "complete_task",
        "Mark a task as done by its numeric id",
        json!({"id": {"type": "integer", "description": "Task id from add_task/list_tasks"}}),
        move |args| {
            let store = complete_store.clone();
            async move {
                let id = args["id"].as_u64().unwrap_or(0);
                if store.complete(id) {
                    Ok(CallToolResult::text(format!("Task #{id} completed")))
                } else {
                    Ok(CallToolResult::error(format!("No task with id {id}")))
                }
            }
        },
    );

    let list_store = store.clone();
    let list_tasks = tool("list_tasks", "List all tasks", json!({}), move |_args| {
        let store = list_store.clone();
        async move { Ok(CallToolResult::text(store.render())) }
    });

    create_sdk_mcp_server(
        "tracker",
        "1.0.0",
        vec![add_task, complete_task, list_tasks],
    )
}

/// A PreToolUse hook: log every tool call, and deny anything that could
/// modify the filesystem (this example is read-only by policy).
fn audit_hook() -> HookCallback {
    Arc::new(|input: HookInput, _tool_use_id, _context| {
        Box::pin(async move {
            let HookInput::PreToolUse(pre) = &input else {
                return Ok(HookJsonOutput::default());
            };
            println!("  [hook] PreToolUse: {}", pre.tool_name);
            if matches!(pre.tool_name.as_str(), "Write" | "Edit" | "Bash") {
                return Ok(HookJsonOutput::Sync(SyncHookJsonOutput {
                    hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                        permission_decision: Some(PermissionDecision::Deny),
                        permission_decision_reason: Some("This example is read-only".to_string()),
                        updated_input: None,
                        additional_context: None,
                    }),
                    ..Default::default()
                }));
            }
            Ok(HookJsonOutput::default())
        })
    })
}

fn print_message(message: &Message) {
    match message {
        Message::Assistant(assistant) => {
            for block in &assistant.content {
                match block {
                    ContentBlock::Text(text) => println!("claude: {}", text.text.trim()),
                    ContentBlock::ToolUse(tool_use) => {
                        println!("  [tool] {} {}", tool_use.name, tool_use.input)
                    }
                    _ => {}
                }
            }
        }
        Message::Result(result) => {
            println!(
                "  [turn done] {} turns, cost ${:.4}",
                result.num_turns,
                result.total_cost_usd.unwrap_or(0.0)
            );
        }
        _ => {}
    }
}

async fn run_turn(client: &ClaudeSdkClient, prompt: &str) -> claude_agent_sdk::Result<()> {
    println!("\nuser: {prompt}");
    client.query(QueryPrompt::from(prompt), None).await?;
    let mut responses = client.receive_response();
    while let Some(message) = responses.next().await {
        print_message(&message?);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let store = Arc::new(TaskStore::default());

    let options = ClaudeAgentOptions {
        system_prompt: Some(
            "You are a project assistant for a Rust crate. Use the tracker tools to \
             manage the task list. Be concise."
                .into(),
        ),
        // Base built-in tool set: read-only exploration only.
        tools: Some(ToolsConfig::List(vec![
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
        ])),
        mcp_servers: McpServers::Map(HashMap::from([(
            "tracker".to_string(),
            McpServerConfig::Sdk(tracker_server(store.clone())),
        )])),
        // The in-process tools run without prompting; built-in tools fall
        // through to the can_use_tool callback below.
        allowed_tools: vec![
            "mcp__tracker__add_task".into(),
            "mcp__tracker__complete_task".into(),
            "mcp__tracker__list_tasks".into(),
        ],
        hooks: Some(HashMap::from([(
            HookEvent::PreToolUse,
            vec![HookMatcher {
                matcher: None, // every tool
                hooks: vec![audit_hook()],
                timeout: None,
            }],
        )])),
        can_use_tool: Some(Arc::new(|tool_name, _input, _context| {
            Box::pin(async move {
                // Read-only built-ins are fine; anything else is denied.
                if matches!(tool_name.as_str(), "Read" | "Glob" | "Grep") {
                    println!("  [permission] allowing {tool_name}");
                    Ok(PermissionResult::allow())
                } else {
                    println!("  [permission] denying {tool_name}");
                    Ok(PermissionResult::deny("Only read-only tools are allowed"))
                }
            })
        })),
        max_turns: Some(16),
        ..Default::default()
    };

    let mut client = ClaudeSdkClient::new(options);
    client.connect(None).await?;

    run_turn(
        &client,
        "List the *.rs files under tests/ in this crate with Glob, then create one \
         tracker task per test file titled 'Review <filename>'. Use priority high for \
         anything control-protocol or transport related, medium otherwise.",
    )
    .await?;

    run_turn(
        &client,
        "Read the first 30 lines of tests/mcp_server_test.rs to see what it covers, \
         then complete the tracker task for that file and show me the task list.",
    )
    .await?;

    client.disconnect().await?;

    // The tool calls above mutated OUR process state — print it directly.
    println!("\nFinal task list (read from this process's memory):");
    println!("{}", store.render());
    Ok(())
}
