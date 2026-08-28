//! In-process (SDK) MCP server support.
//!
//! Unlike external MCP servers that run as separate processes, SDK MCP
//! servers run directly in your application's process: the CLI speaks
//! JSON-RPC to them through the SDK's control channel, and the SDK routes
//! each message to the configured [`SdkMcpServer`]. This provides better
//! performance (no IPC overhead), simpler deployment, easier debugging, and
//! direct access to your application's state.
//!
//! The Python SDK bridges these messages into an `mcp.server.Server`; this
//! crate instead ships a native tools-only server ([`create_sdk_mcp_server`])
//! and lets you bring your own JSON-RPC handler by implementing
//! [`SdkMcpServer`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::errors::Result;
use crate::types::McpSdkServerConfig;

/// MCP protocol version answered to `initialize` requests that don't name
/// one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC "invalid params".
const INVALID_PARAMS: i64 = -32602;

/// Hints about a tool's behavior, plus `max_result_size_chars`.
///
/// The hints (`read_only_hint`, `destructive_hint`, ...) are standard MCP
/// tool annotations advertised to Claude. `max_result_size_chars` is not an
/// MCP hint but a Claude Code one: the size, in characters, up to which
/// Claude Code keeps this tool's result inline instead of persisting it to a
/// file and showing a preview. It travels to the CLI in the tool's `_meta`.
///
/// Serializes to the camelCase wire spelling (`readOnlyHint`, ...).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// Human-readable title for the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The tool does not modify its environment.
    #[serde(
        rename = "readOnlyHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only_hint: Option<bool>,
    /// The tool may perform destructive updates.
    #[serde(
        rename = "destructiveHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub destructive_hint: Option<bool>,
    /// Repeated calls with the same arguments have no additional effect.
    #[serde(
        rename = "idempotentHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotent_hint: Option<bool>,
    /// The tool interacts with an open world of external entities.
    #[serde(
        rename = "openWorldHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_world_hint: Option<bool>,
    /// Size, in characters, up to which Claude Code keeps this tool's result
    /// inline instead of persisting it to a file and showing a preview.
    #[serde(
        rename = "maxResultSizeChars",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_result_size_chars: Option<u64>,
    /// Additional annotation fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One content item in a tool handler's result.
///
/// Text and image blocks pass through to the CLI. Resource links and text
/// resources are flattened to text because that is what the CLI renders;
/// binary resources and unknown block types are dropped with a warning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    /// A text block.
    #[serde(rename = "text")]
    Text {
        /// The text.
        text: String,
    },
    /// An image block.
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image data.
        data: String,
        /// Image MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// A resource link; flattened to text for the CLI.
    #[serde(rename = "resource_link")]
    ResourceLink {
        /// Resource name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Resource URI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        /// Resource description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// An embedded resource; its text is flattened for the CLI, binary
    /// resources are dropped.
    #[serde(rename = "resource")]
    Resource {
        /// The raw resource object.
        resource: Value,
    },
}

impl ToolResultContent {
    /// A text content item.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Result of an SDK MCP tool handler.
#[derive(Debug, Clone, Default)]
pub struct CallToolResult {
    /// Result content items.
    pub content: Vec<ToolResultContent>,
    /// Whether the result is an error the model should read as such.
    pub is_error: bool,
}

impl CallToolResult {
    /// A successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: false,
        }
    }

    /// An error text result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: true,
        }
    }
}

/// Handler for an SDK MCP tool. Receives the tool arguments as a JSON value;
/// returning `Err` is reported to Claude as an error result rather than
/// failing the request.
pub type ToolHandler =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<CallToolResult>> + Send + Sync>;

/// Definition for an SDK MCP tool.
#[derive(Clone)]
pub struct SdkMcpTool {
    /// Unique identifier for the tool. This is what Claude uses to reference
    /// the tool in function calls.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// Schema defining the tool's input parameters. Either a full JSON
    /// Schema object (`{"type": "object", "properties": {...}}`) or a map of
    /// parameter names to JSON-schema fragments (e.g. `{"text": {"type":
    /// "string"}}`), which is wrapped into an object schema with every
    /// parameter required — mirroring the Python SDK's dict-style shorthand.
    pub input_schema: Value,
    /// The tool implementation.
    pub handler: ToolHandler,
    /// Optional MCP tool annotations (hints such as `read_only_hint` or
    /// `destructive_hint`) advertised to Claude.
    /// `ToolAnnotations { max_result_size_chars: Some(n), .. }` additionally
    /// raises the size up to which Claude Code keeps this tool's result
    /// inline instead of persisting it to a file and showing a preview.
    pub annotations: Option<ToolAnnotations>,
}

impl std::fmt::Debug for SdkMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkMcpTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("annotations", &self.annotations)
            .finish_non_exhaustive()
    }
}

/// Define an SDK MCP tool. The counterpart of the Python SDK's `@tool`
/// decorator.
///
/// The tool runs in-process within your application, providing better
/// performance than external MCP servers. Arguments are validated against
/// `input_schema` before the handler is called; invalid input, unknown
/// tools, and handler errors are all reported back to Claude as error
/// results rather than failing the request.
///
/// # Example
///
/// ```no_run
/// use clawde::mcp::{tool, CallToolResult};
/// use serde_json::json;
///
/// let greet = tool(
///     "greet",
///     "Greet a user",
///     json!({"name": {"type": "string"}}),
///     |args| async move {
///         let name = args["name"].as_str().unwrap_or("world");
///         Ok(CallToolResult::text(format!("Hello, {name}!")))
///     },
/// );
/// ```
pub fn tool<F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    input_schema: Value,
    handler: F,
) -> SdkMcpTool
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<CallToolResult>> + Send + 'static,
{
    SdkMcpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
        handler: Arc::new(move |args| Box::pin(handler(args))),
        annotations: None,
    }
}

impl SdkMcpTool {
    /// Attach [`ToolAnnotations`] to this tool.
    pub fn with_annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }
}

/// An in-process MCP server the SDK serves to Claude Code over the control
/// protocol.
///
/// Implement this to bring your own server; [`create_sdk_mcp_server`]
/// provides a tools-only implementation. Requests and notifications a server
/// initiates toward the client (sampling, elicitation, roots, logging,
/// progress) are not forwarded.
#[async_trait]
pub trait SdkMcpServer: Send + Sync {
    /// Handle one JSON-RPC message from the CLI.
    ///
    /// Return the JSON-RPC response object for requests, and `None` for
    /// notifications and responses (which expect no reply). Errors are
    /// reported to the CLI as JSON-RPC error responses.
    async fn handle_message(&self, message: Value) -> Result<Option<Value>>;

    /// Called when the query shuts down, so a server holding resources
    /// (connections, files, running work) can release them — the counterpart
    /// of the Python SDK's bridge close. The SDK bounds the wait to a short
    /// grace period, mirroring the Python bridge's shutdown grace. The
    /// default does nothing.
    async fn close(&self) {}
}

/// Turn a tool's declared `input_schema` into the JSON Schema sent on the
/// wire. Mirrors the Python SDK's `_build_input_schema`.
fn build_input_schema(input_schema: &Value) -> Value {
    if let Value::Object(map) = input_schema {
        let is_full_schema =
            map.get("type").is_some_and(Value::is_string) && map.contains_key("properties");
        if is_full_schema {
            return input_schema.clone();
        }
        let properties: Map<String, Value> =
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let required: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
        return json!({
            "type": "object",
            "properties": properties,
            "required": required,
        });
    }
    json!({"type": "object", "properties": {}})
}

/// Client-specific hints travel in `_meta` under namespaced keys because MCP
/// clients drop annotation fields they do not know.
fn build_meta(annotations: Option<&ToolAnnotations>) -> Option<Value> {
    let max_size = annotations?.max_result_size_chars?;
    Some(json!({"anthropic/maxResultSizeChars": max_size}))
}

struct RegisteredTool {
    tool: SdkMcpTool,
    schema: Value,
    // Err carries the schema compile error: calls against such a tool fail
    // as isError results without running the handler (never silently
    // unvalidated), matching the Python SDK where jsonschema raises at
    // validation time inside the guarded call path.
    validator: std::result::Result<jsonschema::Validator, String>,
}

/// The tools-only [`SdkMcpServer`] produced by [`create_sdk_mcp_server`].
pub struct ToolsMcpServer {
    name: String,
    version: String,
    tools: HashMap<String, RegisteredTool>,
    tool_order: Vec<String>,
}

impl ToolsMcpServer {
    fn new(name: String, version: String, tools: Vec<SdkMcpTool>) -> Self {
        let mut registered = HashMap::new();
        let mut tool_order = Vec::new();
        for tool_def in tools {
            let schema = build_input_schema(&tool_def.input_schema);
            let validator = jsonschema::validator_for(&schema).map_err(|e| {
                tracing::warn!(
                    target: "clawde",
                    "invalid input schema for tool {:?}: {e}; calls to it will fail",
                    tool_def.name
                );
                e.to_string()
            });
            tool_order.push(tool_def.name.clone());
            registered.insert(
                tool_def.name.clone(),
                RegisteredTool {
                    tool: tool_def,
                    schema,
                    validator,
                },
            );
        }
        Self {
            name,
            version,
            tools: registered,
            tool_order,
        }
    }

    fn wire_tools(&self) -> Vec<Value> {
        self.tool_order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|reg| {
                let mut obj = json!({
                    "name": reg.tool.name,
                    "description": reg.tool.description,
                    "inputSchema": reg.schema,
                });
                if let Some(annotations) = &reg.tool.annotations {
                    obj["annotations"] = serde_json::to_value(annotations).unwrap_or(Value::Null);
                }
                if let Some(meta) = build_meta(reg.tool.annotations.as_ref()) {
                    obj["_meta"] = meta;
                }
                obj
            })
            .collect()
    }

    /// Map the content of a tool handler's result to MCP content blocks.
    /// Mirrors the Python SDK's `_convert_tool_content`.
    fn convert_tool_content(items: Vec<ToolResultContent>) -> Vec<Value> {
        let mut content = Vec::new();
        for item in items {
            match item {
                ToolResultContent::Text { text } => {
                    content.push(json!({"type": "text", "text": text}));
                }
                ToolResultContent::Image { data, mime_type } => {
                    content.push(json!({"type": "image", "data": data, "mimeType": mime_type}));
                }
                ToolResultContent::ResourceLink {
                    name,
                    uri,
                    description,
                } => {
                    let parts: Vec<String> = [name, uri, description]
                        .into_iter()
                        .flatten()
                        .filter(|p| !p.is_empty())
                        .collect();
                    let text = if parts.is_empty() {
                        "Resource link".to_string()
                    } else {
                        parts.join("\n")
                    };
                    content.push(json!({"type": "text", "text": text}));
                }
                ToolResultContent::Resource { resource } => {
                    if let Some(text) = resource.get("text").and_then(Value::as_str) {
                        content.push(json!({"type": "text", "text": text}));
                    } else {
                        tracing::warn!(
                            target: "clawde",
                            "Binary embedded resource cannot be converted to text, skipping"
                        );
                    }
                }
            }
        }
        content
    }

    fn error_call_result(message: &str) -> Value {
        json!({
            "content": [{"type": "text", "text": message}],
            "isError": true,
        })
    }

    /// Run a tool with the SDK-owned error semantics: unknown tools, invalid
    /// arguments and handler failures all come back as `isError` results the
    /// model can read, never as protocol errors.
    async fn run_tool(&self, tool_name: &str, arguments: Value) -> Value {
        let Some(reg) = self.tools.get(tool_name) else {
            return Self::error_call_result(&format!("Tool '{tool_name}' not found"));
        };
        match &reg.validator {
            Ok(validator) => {
                if let Err(error) = validator.validate(&arguments) {
                    return Self::error_call_result(&format!("Input validation error: {error}"));
                }
            }
            // The tool's declared schema doesn't compile: never run the
            // handler unvalidated — report the schema problem to the model.
            Err(schema_error) => {
                return Self::error_call_result(&format!(
                    "Input validation error: invalid input schema for tool \
                     '{tool_name}': {schema_error}"
                ));
            }
        }
        match (reg.tool.handler)(arguments).await {
            Ok(result) => json!({
                "content": Self::convert_tool_content(result.content),
                "isError": result.is_error,
            }),
            Err(e) => Self::error_call_result(&e.to_string()),
        }
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[async_trait]
impl SdkMcpServer for ToolsMcpServer {
    async fn handle_message(&self, message: Value) -> Result<Option<Value>> {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        let Some(method) = method else {
            // A response (or malformed frame) — nothing to send back.
            return Ok(None);
        };
        let Some(id) = id else {
            // A notification; nothing to reply. `notifications/initialized`
            // and friends are all accepted silently.
            return Ok(None);
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": self.name, "version": self.version},
                    }),
                )
            }
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({"tools": self.wire_tools()})),
            "tools/call" => {
                let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
                    return Ok(Some(rpc_error(
                        id,
                        INVALID_PARAMS,
                        "tools/call requires a 'name' parameter".to_string(),
                    )));
                };
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                rpc_result(id, self.run_tool(tool_name, arguments).await)
            }
            other => rpc_error(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
        };
        Ok(Some(response))
    }
}

/// Create an in-process MCP server that runs within your application.
///
/// The returned [`McpSdkServerConfig`] can be used as an
/// [`crate::McpServerConfig::Sdk`] entry in
/// [`crate::ClaudeAgentOptions::mcp_servers`]. Its `instance` is a
/// [`ToolsMcpServer`] the SDK serves to Claude Code over the control
/// protocol; any [`SdkMcpServer`] you build yourself can be used in its
/// place.
///
/// # Example
///
/// ```no_run
/// use std::collections::HashMap;
/// use clawde::mcp::{create_sdk_mcp_server, tool, CallToolResult};
/// use clawde::{ClaudeAgentOptions, McpServerConfig, McpServers};
/// use serde_json::json;
///
/// let add = tool(
///     "add",
///     "Add numbers",
///     json!({"a": {"type": "number"}, "b": {"type": "number"}}),
///     |args| async move {
///         let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
///         Ok(CallToolResult::text(format!("Sum: {sum}")))
///     },
/// );
/// let calculator = create_sdk_mcp_server("calculator", "2.0.0", vec![add]);
/// let options = ClaudeAgentOptions {
///     mcp_servers: McpServers::Map(HashMap::from([(
///         "calc".to_string(),
///         McpServerConfig::Sdk(calculator),
///     )])),
///     allowed_tools: vec!["mcp__calc__add".to_string()],
///     ..Default::default()
/// };
/// ```
pub fn create_sdk_mcp_server(
    name: impl Into<String>,
    version: impl Into<String>,
    tools: Vec<SdkMcpTool>,
) -> McpSdkServerConfig {
    let name = name.into();
    let server = ToolsMcpServer::new(name.clone(), version.into(), tools);
    McpSdkServerConfig {
        name,
        instance: Arc::new(server),
    }
}
