//! Error types for the Claude Agent SDK.
//!
//! Mirrors the Python SDK's exception hierarchy (`ClaudeSDKError`,
//! `CLIConnectionError`, `CLINotFoundError`, `ProcessError`, `ResultError`,
//! `CLIJSONDecodeError`, `MessageParseError`) as one idiomatic error enum.

use serde_json::Value;

/// Convenience alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, ClaudeSdkError>;

/// All errors produced by the Claude Agent SDK.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeSdkError {
    /// Unable to connect to Claude Code.
    #[error("{message}")]
    CliConnection {
        /// Human-readable description of the connection failure.
        message: String,
    },

    /// Claude Code is not found or not installed.
    #[error("{message}{}", cli_path.as_deref().map(|p| format!(": {p}")).unwrap_or_default())]
    CliNotFound {
        /// Human-readable description.
        message: String,
        /// Path that was probed, if any.
        cli_path: Option<String>,
    },

    /// The CLI process failed.
    #[error("{}", process_error_text(message, *exit_code, stderr.as_deref()))]
    Process {
        /// Base error message.
        message: String,
        /// Process exit code, if known.
        exit_code: Option<i32>,
        /// Captured stderr output, if any.
        stderr: Option<String>,
    },

    /// The CLI exited after reporting a terminal error result.
    ///
    /// The CLI ends a failed run by emitting a `result` message with
    /// `is_error: true` and then exiting non-zero. This error replaces the
    /// bare "exit code 1" [`ClaudeSdkError::Process`] for that case and
    /// carries the result's payload, so callers can branch on *why* the run
    /// failed without string matching.
    #[error("{}", process_error_text(message, *exit_code, None))]
    ResultError {
        /// Base error message.
        message: String,
        /// Process exit code, if known.
        exit_code: Option<i32>,
        /// The raw `result` message payload as emitted by the CLI.
        data: Value,
    },

    /// Unable to decode JSON from CLI output.
    #[error("Failed to decode JSON: {}...", truncate(line, 100))]
    JsonDecode {
        /// The offending line.
        line: String,
        /// Description of the original parse error.
        original_error: String,
    },

    /// Unable to parse a message from CLI output.
    #[error("{message}")]
    MessageParse {
        /// Description of the parse failure.
        message: String,
        /// The raw message data, if available.
        data: Option<Value>,
    },

    /// A control request failed or timed out, or another protocol-level
    /// failure occurred.
    #[error("{0}")]
    ControlProtocol(String),

    /// Invalid configuration or argument (Python raises `ValueError`).
    #[error("{0}")]
    InvalidConfig(String),

    /// An optional [`crate::SessionStore`] method is not implemented by this
    /// store. Mirrors the Python `SessionStore` protocol defaults that raise
    /// `NotImplementedError`; SDK call sites probe for this variant.
    #[error("session store does not implement {method}()")]
    StoreUnimplemented {
        /// The optional store method that is absent.
        method: &'static str,
    },

    /// A filesystem session operation could not find the session
    /// (Python raises `FileNotFoundError`).
    #[error("{0}")]
    SessionNotFound(String),

    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Any other error, including failures from user-supplied callbacks and
    /// session store adapters.
    #[error("{0}")]
    Other(String),
}

impl ClaudeSdkError {
    /// Shorthand constructor for [`ClaudeSdkError::CliConnection`].
    pub fn cli_connection(message: impl Into<String>) -> Self {
        Self::CliConnection {
            message: message.into(),
        }
    }

    /// Shorthand constructor for [`ClaudeSdkError::CliNotFound`].
    pub fn cli_not_found(message: impl Into<String>, cli_path: Option<String>) -> Self {
        Self::CliNotFound {
            message: message.into(),
            cli_path,
        }
    }

    /// Shorthand constructor for [`ClaudeSdkError::Other`].
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// The result subtype (`"error_max_turns"`, `"error_during_execution"`,
    /// ... — or `"success"` when the agent loop itself completed but the last
    /// turn was an API error). Only present on [`ClaudeSdkError::ResultError`].
    pub fn subtype(&self) -> Option<&str> {
        self.result_str("subtype")
    }

    /// Error strings reported by the CLI (may be empty). Only meaningful on
    /// [`ClaudeSdkError::ResultError`].
    pub fn errors(&self) -> Vec<String> {
        match self {
            Self::ResultError { data, .. } => normalize_result_errors(data.get("errors")),
            _ => Vec::new(),
        }
    }

    /// The result text, if any. For API failures this holds the
    /// `"API Error: ..."` prose. Only present on [`ClaudeSdkError::ResultError`].
    pub fn result(&self) -> Option<&str> {
        self.result_str("result")
    }

    /// HTTP status of the failing API call, if any. Only present on
    /// [`ClaudeSdkError::ResultError`].
    pub fn api_error_status(&self) -> Option<i64> {
        match self {
            Self::ResultError { data, .. } => data.get("api_error_status").and_then(Value::as_i64),
            _ => None,
        }
    }

    /// Why the run ended (e.g. `"api_error"`, `"max_turns"`), if reported by
    /// the CLI. Only present on [`ClaudeSdkError::ResultError`].
    pub fn terminal_reason(&self) -> Option<&str> {
        self.result_str("terminal_reason")
    }

    /// Session the result belongs to, if reported. Only present on
    /// [`ClaudeSdkError::ResultError`].
    pub fn session_id(&self) -> Option<&str> {
        self.result_str("session_id")
    }

    /// The process exit code carried by [`ClaudeSdkError::Process`] or
    /// [`ClaudeSdkError::ResultError`].
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Process { exit_code, .. } | Self::ResultError { exit_code, .. } => *exit_code,
            _ => None,
        }
    }

    fn result_str(&self, key: &str) -> Option<&str> {
        match self {
            Self::ResultError { data, .. } => data.get(key).and_then(Value::as_str),
            _ => None,
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn process_error_text(message: &str, exit_code: Option<i32>, stderr: Option<&str>) -> String {
    let mut text = message.to_string();
    if let Some(code) = exit_code {
        text = format!("{text} (exit code: {code})");
    }
    if let Some(err) = stderr {
        if !err.is_empty() {
            text = format!("{text}\nError output: {err}");
        }
    }
    text
}

/// Normalize the `errors` field of a `result` frame to clean strings.
///
/// The CLI emits a list of strings; tolerate a bare string (older/buggy
/// emitters) and drop non-string or blank entries so the structured errors
/// and the error text always agree.
pub(crate) fn normalize_result_errors(raw: Option<&Value>) -> Vec<String> {
    let items: Vec<&Value> = match raw {
        Some(Value::String(_)) => vec![raw.unwrap()],
        Some(Value::Array(list)) => list.iter().collect(),
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
