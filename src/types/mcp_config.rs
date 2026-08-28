//! MCP server configuration and status types.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::mcp::SdkMcpServer;

/// MCP stdio server configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpStdioServerConfig {
    /// Executable to spawn.
    pub command: String,
    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the server process.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// MCP SSE server configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpSseServerConfig {
    /// Server URL.
    pub url: String,
    /// Extra HTTP headers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

/// MCP HTTP server configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpHttpServerConfig {
    /// Server URL.
    pub url: String,
    /// Extra HTTP headers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

/// SDK (in-process) MCP server configuration.
///
/// Usually produced by [`crate::create_sdk_mcp_server`]. `instance` may also
/// be any [`SdkMcpServer`] implementation you have built yourself; the SDK
/// serves it to Claude Code over the control protocol's in-process MCP
/// transport.
#[derive(Clone)]
pub struct McpSdkServerConfig {
    /// Unique identifier for the server.
    pub name: String,
    /// The in-process server instance.
    pub instance: Arc<dyn SdkMcpServer>,
}

impl std::fmt::Debug for McpSdkServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSdkServerConfig")
            .field("name", &self.name)
            .field("instance", &"<SdkMcpServer>")
            .finish()
    }
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub enum McpServerConfig {
    /// External server spawned over stdio.
    Stdio(McpStdioServerConfig),
    /// External server reached over SSE.
    Sse(McpSseServerConfig),
    /// External server reached over HTTP.
    Http(McpHttpServerConfig),
    /// In-process SDK server.
    Sdk(McpSdkServerConfig),
}

impl McpServerConfig {
    /// The JSON shape sent to the CLI in `--mcp-config`. SDK servers are
    /// stripped to `{type, name}` — the in-process instance never crosses the
    /// process boundary.
    pub(crate) fn to_cli_json(&self) -> Value {
        match self {
            Self::Stdio(c) => {
                let mut v = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("type".into(), json!("stdio"));
                }
                v
            }
            Self::Sse(c) => {
                let mut v = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("type".into(), json!("sse"));
                }
                v
            }
            Self::Http(c) => {
                let mut v = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("type".into(), json!("http"));
                }
                v
            }
            Self::Sdk(c) => json!({"type": "sdk", "name": c.name}),
        }
    }
}

/// MCP server configurations: either a map of name → config, or a path to an
/// MCP config JSON file (or an inline JSON string).
#[derive(Debug, Clone)]
pub enum McpServers {
    /// Map of server name → configuration.
    Map(HashMap<String, McpServerConfig>),
    /// Path to an MCP config JSON file, or an inline JSON string.
    Path(String),
}

impl Default for McpServers {
    fn default() -> Self {
        Self::Map(HashMap::new())
    }
}

impl McpServers {
    /// Whether no servers are configured.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Map(map) => map.is_empty(),
            Self::Path(path) => path.is_empty(),
        }
    }

    /// The SDK (in-process) servers configured in this map.
    pub(crate) fn sdk_servers(&self) -> HashMap<String, Arc<dyn SdkMcpServer>> {
        match self {
            Self::Map(map) => map
                .iter()
                .filter_map(|(name, config)| match config {
                    McpServerConfig::Sdk(sdk) => Some((name.clone(), sdk.instance.clone())),
                    _ => None,
                })
                .collect(),
            Self::Path(_) => HashMap::new(),
        }
    }
}

/// Connection status values for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerConnectionStatus {
    /// Server is connected.
    #[serde(rename = "connected")]
    Connected,
    /// Server failed to connect.
    #[serde(rename = "failed")]
    Failed,
    /// Server needs authentication.
    #[serde(rename = "needs-auth")]
    NeedsAuth,
    /// Connection is pending.
    #[serde(rename = "pending")]
    Pending,
    /// Server is disabled.
    #[serde(rename = "disabled")]
    Disabled,
    /// A status this SDK version doesn't know.
    #[serde(untagged)]
    Other(String),
}

/// Tool annotations as returned in MCP server status. Wire format uses
/// camelCase field names (from CLI JSON output).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpToolAnnotations {
    /// Whether the tool is read-only.
    #[serde(rename = "readOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Whether the tool is destructive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    /// Whether the tool touches the open world.
    #[serde(rename = "openWorld", skip_serializing_if = "Option::is_none")]
    pub open_world: Option<bool>,
}

/// Information about a tool provided by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    /// Tool name.
    pub name: String,
    /// Tool description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tool annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

/// Server info from the MCP initialize handshake (available when connected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

/// Status information for an MCP server connection.
///
/// Returned by [`crate::ClaudeSdkClient::get_mcp_status`] in the
/// `mcp_servers` list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// Server name as configured.
    pub name: String,
    /// Current connection status.
    pub status: McpServerConnectionStatus,
    /// Server information from the MCP handshake (available when connected).
    #[serde(
        rename = "serverInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_info: Option<McpServerInfo>,
    /// Error message (available when status is `Failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Server configuration (includes URL for HTTP/SSE servers). Kept as raw
    /// JSON — the shape is a union of stdio/sse/http/sdk/claudeai-proxy
    /// configs and may grow new variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// Configuration scope (e.g. project, user, local, claudeai, managed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Tools provided by this server (available when connected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolInfo>>,
    /// Fields not modeled by this SDK version.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Response from [`crate::ClaudeSdkClient::get_mcp_status`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpStatusResponse {
    /// Status of each configured MCP server.
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Vec<McpServerStatus>,
}
