//! Subprocess transport implementation using the Claude Code CLI.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::Transport;
use crate::errors::{ClaudeSdkError, Result};
use crate::types::options::SkillsConfig;
use crate::types::{ClaudeAgentOptions, McpServers, SystemPrompt, ThinkingConfig, ToolsConfig};

const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024; // 1MB buffer limit
const MINIMUM_CLAUDE_CODE_VERSION: [u32; 3] = [2, 0, 0];

/// cmd.exe metacharacters (plus the quote character cmd.exe uses to toggle
/// its quoting state, and "!", which expands like "%" when delayed expansion
/// is enabled). Argument quoting on Windows follows the MSVCRT argv rules
/// only, so in a whitespace-free argument these characters reach a cmd.exe
/// command line verbatim.
const CMD_EXE_METACHARACTERS: &str = "&|<>^%!\"";

/// The SDK version reported to the CLI.
const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reassembles complete lines from a stream that yields arbitrary chunks.
///
/// Reads yield CHUNKS of BYTES, not lines, so a large line spans several
/// chunks and a chunk boundary can fall anywhere — including inside a JSON
/// string value or in the middle of a multi-byte UTF-8 character. Framing on
/// bytes and decoding only complete lines is what keeps both whitespace and
/// split code points at the seam intact (a per-chunk decode would corrupt a
/// multi-byte character split across two reads).
pub(crate) struct LineFramer {
    // Only ever holds the fragment of the line currently being received,
    // which contains no newline.
    pending: Vec<u8>,
}

impl LineFramer {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Bytes of the trailing partial line buffered so far.
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Add a chunk, returning any lines it completed.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        if !chunk.contains(&b'\n') {
            return Vec::new();
        }
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.pending[start..].iter().position(|b| *b == b'\n') {
            let line = &self.pending[start..start + pos];
            lines.push(String::from_utf8_lossy(line).to_string());
            start += pos + 1;
        }
        self.pending.drain(..start);
        lines
    }

    /// Take the trailing partial line, if any.
    pub(crate) fn flush(&mut self) -> String {
        let line = String::from_utf8_lossy(&self.pending).to_string();
        self.pending.clear();
        line
    }
}

/// Parse one complete line of the CLI's NDJSON stdout.
///
/// Returns `None` for lines that carry no message: blank lines, and non-JSON
/// output such as `[SandboxDebug] ...` that some CLI builds write to stdout.
/// A line that looks like JSON but does not parse is corrupt — with proper
/// line framing there is no later data that could complete it — so it errors
/// rather than silently dropping a message.
fn parse_stdout_line(line: &str) -> Result<Option<Value>> {
    // `line` is a complete line, so surrounding whitespace (e.g. the "\r" of
    // a CRLF) is meaningless. Only chunks must never be stripped.
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    if !line.starts_with('{') {
        tracing::debug!(
            target: "claude_agent_sdk",
            "Skipping non-JSON line from CLI stdout: {}",
            &line[..line.len().min(200)]
        );
        return Ok(None);
    }
    match serde_json::from_str::<Value>(line) {
        Ok(data) => Ok(Some(data)),
        Err(e) => Err(ClaudeSdkError::JsonDecode {
            line: line.to_string(),
            original_error: e.to_string(),
        }),
    }
}

struct StdinState {
    stdin: Option<ChildStdin>,
    ready: bool,
    exit_error: Option<String>,
}

/// Subprocess transport using the Claude Code CLI.
pub struct SubprocessCliTransport {
    options: ClaudeAgentOptions,
    cli_path: Mutex<Option<PathBuf>>,
    cwd: Option<PathBuf>,
    child: Arc<Mutex<Option<Child>>>,
    stdout: Mutex<Option<ChildStdout>>,
    stdin: Arc<Mutex<StdinState>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    ready: AtomicBool,
    max_buffer_size: usize,
}

impl SubprocessCliTransport {
    /// Create a transport for the given options. The subprocess is spawned
    /// by [`Transport::connect`].
    pub fn new(options: ClaudeAgentOptions) -> Self {
        let cli_path = options.cli_path.clone();
        let cwd = options.cwd.clone();
        let max_buffer_size = options.max_buffer_size.unwrap_or(DEFAULT_MAX_BUFFER_SIZE);
        Self {
            options,
            cli_path: Mutex::new(cli_path),
            cwd,
            child: Arc::new(Mutex::new(None)),
            stdout: Mutex::new(None),
            stdin: Arc::new(Mutex::new(StdinState {
                stdin: None,
                ready: false,
                exit_error: None,
            })),
            stderr_task: Mutex::new(None),
            ready: AtomicBool::new(false),
            max_buffer_size,
        }
    }

    /// Find the Claude Code CLI binary.
    fn find_cli() -> Result<PathBuf> {
        let mut which_hit: Option<PathBuf> = None;
        if let Ok(cli) = which::which("claude") {
            if !cfg!(windows) || Self::is_windows_native_exe(&cli) {
                return Ok(cli);
            }
            // Windows resolved something CreateProcess cannot run directly as
            // the CLI: npm's claude.cmd shim (which connect() refuses to
            // spawn) or an extensionless wrapper script. Prefer any
            // discoverable native executable, and keep this hit only as the
            // last resort so a shim-only machine still gets the explanatory
            // batch-script refusal from connect().
            if let Ok(exe) = which::which("claude.exe") {
                if Self::is_windows_native_exe(&exe) {
                    return Ok(exe);
                }
            }
            which_hit = Some(cli);
        }

        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let locations: Vec<PathBuf> = if cfg!(windows) {
            // Only the native installer's claude.exe. The POSIX-shaped
            // entries below are deliberately not probed on Windows: an
            // extensionless match would preempt the explanatory batch-script
            // refusal with an opaque spawn failure, and a rooted-but-driveless
            // "/usr/local/bin/claude" resolves against the current drive — a
            // binary-planting probe.
            vec![home.join(".local/bin/claude.exe")]
        } else {
            vec![
                home.join(".npm-global/bin/claude"),
                PathBuf::from("/usr/local/bin/claude"),
                home.join(".local/bin/claude"),
                home.join("node_modules/.bin/claude"),
                home.join(".yarn/bin/claude"),
                home.join(".claude/local/claude"),
            ]
        };
        for path in locations {
            if path.is_file() {
                return Ok(path);
            }
        }

        if let Some(hit) = which_hit {
            // No native executable was discoverable anywhere: return the
            // original which() hit so connect() raises the batch-script
            // refusal (with its remediation) for a shim, or the spawn error
            // for a wrapper script, rather than a bare not-found error.
            return Ok(hit);
        }

        if cfg!(windows) {
            return Err(ClaudeSdkError::cli_not_found(
                "Claude Code not found. Install the native claude.exe with (PowerShell):\n  \
                 irm https://claude.ai/install.ps1 | iex\n\nOr provide the path to a claude.exe \
                 via ClaudeAgentOptions { cli_path: Some(\"C:\\\\path\\\\to\\\\claude.exe\".into()), .. }\n\n\
                 (npm install -g @anthropic-ai/claude-code produces a claude.cmd shim, which this \
                 SDK refuses to run on Windows.)",
                None,
            ));
        }
        Err(ClaudeSdkError::cli_not_found(
            "Claude Code not found. Install with:\n  npm install -g @anthropic-ai/claude-code\n\n\
             If already installed locally, try:\n  export PATH=\"$HOME/node_modules/.bin:$PATH\"\n\n\
             Or provide the path via ClaudeAgentOptions { cli_path: Some(\"/path/to/claude\".into()), .. }",
            None,
        ))
    }

    /// Whether the path's final component names an image CreateProcess runs
    /// directly (.exe / .com), used only to decide which discovery result to
    /// prefer. It is not a security gate: every returned path still passes
    /// [`Self::reject_windows_batch_cli`] in connect().
    fn is_windows_native_exe(cli_path: &Path) -> bool {
        let path = cli_path.to_string_lossy().replace('\\', "/");
        let name = path.rsplit('/').next().unwrap_or("").to_lowercase();
        let trimmed = name.trim_end_matches(['.', ' ']);
        trimmed.ends_with(".exe") || trimmed.ends_with(".com")
    }

    /// Whether `cli_path` names a .bat/.cmd batch script on Windows. Always
    /// false off Windows.
    ///
    /// Classifies EVERY path component, not only the final one, and splits
    /// each component on `:` (drive prefixes, NTFS stream specs), stripping
    /// trailing dots and spaces per segment — the same normalization Windows
    /// applies at path resolution. Refusing whenever ANY segment carries a
    /// batch extension closes the whole normalization-trick class outright;
    /// it costs nothing legitimate because no real claude.exe lives beneath
    /// a directory named like a batch file.
    fn is_windows_batch_cli(cli_path: &str) -> bool {
        if !cfg!(windows) {
            return false;
        }
        cli_path
            .replace('\\', "/")
            .split('/')
            .flat_map(|component| component.split(':'))
            .any(|segment| {
                let trimmed = segment.trim_end_matches(['.', ' ']).to_lowercase();
                trimmed.ends_with(".bat") || trimmed.ends_with(".cmd")
            })
    }

    /// Refuse to execute a .bat/.cmd script as the CLI on Windows.
    ///
    /// Windows has no shebang mechanism: CreateProcess runs batch scripts by
    /// silently rewriting the spawn into a `cmd.exe /c` invocation, and
    /// cmd.exe re-parses the whole command line at execution time, so cmd.exe
    /// metacharacters inside an argument value can execute injected
    /// commands. Reliable escaping for cmd.exe does not exist, so spawning a
    /// batch script with runtime-provided arguments cannot be made safe.
    /// Refusing is the same remediation Node.js shipped for this
    /// vulnerability class (CVE-2024-27980, "BatBadBut").
    fn reject_windows_batch_cli(cli_path: &Path) -> Result<()> {
        if !Self::is_windows_batch_cli(&cli_path.to_string_lossy()) {
            return Ok(());
        }
        Err(ClaudeSdkError::cli_connection(format!(
            "Refusing to execute batch script {cli_path:?}: Windows runs .bat/.cmd files via \
             cmd.exe, which can execute commands injected through CLI arguments, and no reliable \
             escaping for cmd.exe exists. Use a native claude executable instead: install Claude \
             Code natively (irm https://claude.ai/install.ps1 | iex) or point \
             ClaudeAgentOptions.cli_path at a claude.exe."
        )))
    }

    /// Defense in depth for Windows: reject cmd.exe metacharacters in values
    /// that applications commonly take from external input, so they stay
    /// inert even if a cmd.exe hop is ever reintroduced between the SDK and
    /// the CLI. No format is imposed beyond this, and POSIX behavior is
    /// unchanged.
    fn reject_windows_cmd_metacharacters(option_name: &str, value: &str) -> Result<()> {
        if !cfg!(windows) {
            return Ok(());
        }
        let mut bad: Vec<char> = value
            .chars()
            .filter(|c| CMD_EXE_METACHARACTERS.contains(*c) || *c == '\r' || *c == '\n')
            .collect();
        bad.sort_unstable();
        bad.dedup();
        if bad.is_empty() {
            return Ok(());
        }
        Err(ClaudeSdkError::InvalidConfig(format!(
            "{option_name} value {value:?} contains characters that are unsafe to pass on a \
             Windows command line: {bad:?}"
        )))
    }

    /// Build the `--settings` value, merging sandbox settings if provided.
    ///
    /// Returns either a JSON string (if sandbox is provided or settings is
    /// JSON), a file path (if only a settings path is provided without
    /// sandbox), or `None` if neither is provided.
    fn build_settings_value(&self) -> Option<String> {
        let has_settings = self.options.settings.is_some();
        let has_sandbox = self.options.sandbox.is_some();
        if !has_settings && !has_sandbox {
            return None;
        }
        if has_settings && !has_sandbox {
            return self.options.settings.clone();
        }

        let mut settings_obj = serde_json::Map::new();
        if let Some(settings) = &self.options.settings {
            let settings_str = settings.trim();
            if settings_str.starts_with('{') && settings_str.ends_with('}') {
                match serde_json::from_str::<Value>(settings_str) {
                    Ok(Value::Object(obj)) => settings_obj = obj,
                    _ => {
                        tracing::warn!(
                            target: "claude_agent_sdk",
                            "Failed to parse settings as JSON, treating as file path: {settings_str}"
                        );
                        if let Ok(content) = std::fs::read_to_string(settings_str) {
                            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&content)
                            {
                                settings_obj = obj;
                            }
                        }
                    }
                }
            } else {
                match std::fs::read_to_string(settings_str) {
                    Ok(content) => {
                        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&content) {
                            settings_obj = obj;
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            target: "claude_agent_sdk",
                            "Settings file not found: {settings_str}"
                        );
                    }
                }
            }
        }
        if let Some(sandbox) = &self.options.sandbox {
            settings_obj.insert(
                "sandbox".to_string(),
                serde_json::to_value(sandbox).unwrap_or(Value::Null),
            );
        }
        Some(Value::Object(settings_obj).to_string())
    }

    /// Compute effective `allowed_tools` and `setting_sources` for skills.
    ///
    /// When `skills` is [`SkillsConfig::All`], injects the bare `Skill` tool;
    /// when it is a list, injects `Skill(name)` for each entry. In either
    /// case `setting_sources` defaults to `[user, project]` when unset so
    /// the CLI discovers installed skills without the caller having to wire
    /// up both options manually. `None` is a no-op.
    fn apply_skills_defaults(&self) -> Result<(Vec<String>, Option<Vec<String>>)> {
        let mut allowed_tools = self.options.allowed_tools.clone();
        let mut setting_sources: Option<Vec<String>> = self
            .options
            .setting_sources
            .as_ref()
            .map(|sources| sources.iter().map(|s| s.as_str().to_string()).collect());

        let Some(skills) = &self.options.skills else {
            return Ok((allowed_tools, setting_sources));
        };

        match skills {
            SkillsConfig::All => {
                if !allowed_tools.iter().any(|t| t == "Skill") {
                    allowed_tools.push("Skill".to_string());
                }
            }
            SkillsConfig::List(names) => {
                for name in names {
                    validate_skill_name(name)?;
                    let pattern = format!("Skill({name})");
                    if !allowed_tools.contains(&pattern) {
                        allowed_tools.push(pattern);
                    }
                }
            }
        }

        if setting_sources.is_none() {
            setting_sources = Some(vec!["user".to_string(), "project".to_string()]);
        }
        Ok((allowed_tools, setting_sources))
    }

    /// Build the CLI command with arguments.
    fn build_command(&self, cli_path: &Path) -> Result<Vec<String>> {
        let mut cmd: Vec<String> = vec![
            cli_path.to_string_lossy().to_string(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];

        match &self.options.system_prompt {
            None => {
                cmd.push("--system-prompt".into());
                cmd.push(String::new());
            }
            Some(SystemPrompt::Text(text)) => {
                cmd.push("--system-prompt".into());
                cmd.push(text.clone());
            }
            Some(SystemPrompt::File { path }) => {
                cmd.push("--system-prompt-file".into());
                cmd.push(path.clone());
            }
            Some(SystemPrompt::Preset { append, .. }) => {
                if let Some(append) = append {
                    cmd.push("--append-system-prompt".into());
                    cmd.push(append.clone());
                }
            }
        }

        if let Some(tools) = &self.options.tools {
            match tools {
                ToolsConfig::List(list) => {
                    cmd.push("--tools".into());
                    cmd.push(list.join(","));
                }
                // 'claude_code' preset maps to 'default'.
                ToolsConfig::Preset => {
                    cmd.push("--tools".into());
                    cmd.push("default".into());
                }
            }
        }

        let (effective_allowed_tools, effective_setting_sources) = self.apply_skills_defaults()?;

        if !effective_allowed_tools.is_empty() {
            cmd.push("--allowedTools".into());
            cmd.push(effective_allowed_tools.join(","));
        }

        if let Some(max_turns) = self.options.max_turns {
            if max_turns != 0 {
                cmd.push("--max-turns".into());
                cmd.push(max_turns.to_string());
            }
        }

        if let Some(max_budget) = self.options.max_budget_usd {
            cmd.push("--max-budget-usd".into());
            cmd.push(max_budget.to_string());
        }

        if !self.options.disallowed_tools.is_empty() {
            cmd.push("--disallowedTools".into());
            cmd.push(self.options.disallowed_tools.join(","));
        }

        if let Some(task_budget) = &self.options.task_budget {
            cmd.push("--task-budget".into());
            cmd.push(task_budget.total.to_string());
        }

        if let Some(model) = &self.options.model {
            cmd.push("--model".into());
            cmd.push(model.clone());
        }

        if let Some(fallback_model) = &self.options.fallback_model {
            cmd.push("--fallback-model".into());
            cmd.push(fallback_model.clone());
        }

        if !self.options.betas.is_empty() {
            cmd.push("--betas".into());
            cmd.push(
                self.options
                    .betas
                    .iter()
                    .map(|b| b.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }

        if let Some(tool_name) = &self.options.permission_prompt_tool_name {
            cmd.push("--permission-prompt-tool".into());
            cmd.push(tool_name.clone());
        }

        if let Some(mode) = self.options.permission_mode {
            cmd.push("--permission-mode".into());
            cmd.push(mode.as_str().to_string());
        }

        if self.options.continue_conversation {
            cmd.push("--continue".into());
        }

        // Pass these as --flag=value rather than as two argv tokens. The CLI
        // declares --resume with an optional value, so in the two-token form
        // a dash-leading value is not bound to the flag and is instead parsed
        // as a separate CLI flag — letting an untrusted value inject
        // arbitrary flags. The equals form always binds the value.
        if let Some(resume) = &self.options.resume {
            Self::reject_windows_cmd_metacharacters("resume", resume)?;
            cmd.push(format!("--resume={resume}"));
        }

        if let Some(session_id) = &self.options.session_id {
            Self::reject_windows_cmd_metacharacters("session_id", session_id)?;
            cmd.push(format!("--session-id={session_id}"));
        }

        if let Some(settings_value) = self.build_settings_value() {
            if !settings_value.is_empty() {
                cmd.push("--settings".into());
                cmd.push(settings_value);
            }
        }

        for directory in &self.options.add_dirs {
            cmd.push("--add-dir".into());
            cmd.push(directory.to_string_lossy().to_string());
        }

        match &self.options.mcp_servers {
            McpServers::Map(servers) if !servers.is_empty() => {
                // SDK servers are stripped to their serializable fields; the
                // in-process instance stays on this side of the boundary.
                let servers_for_cli: serde_json::Map<String, Value> = servers
                    .iter()
                    .map(|(name, config)| (name.clone(), config.to_cli_json()))
                    .collect();
                cmd.push("--mcp-config".into());
                cmd.push(json!({ "mcpServers": servers_for_cli }).to_string());
            }
            McpServers::Path(path) if !path.is_empty() => {
                cmd.push("--mcp-config".into());
                cmd.push(path.clone());
            }
            _ => {}
        }

        if self.options.include_partial_messages {
            cmd.push("--include-partial-messages".into());
        }

        if self.options.include_hook_events {
            cmd.push("--include-hook-events".into());
        }

        if self.options.strict_mcp_config {
            cmd.push("--strict-mcp-config".into());
        }

        if self.options.fork_session {
            cmd.push("--fork-session".into());
        }

        // Equals form so the value can never be parsed as a separate flag,
        // even if the CLI's declaration of these options ever changes.
        if let Some(resume_session_at) = &self.options.resume_session_at {
            Self::reject_windows_cmd_metacharacters("resume_session_at", resume_session_at)?;
            cmd.push(format!("--resume-session-at={resume_session_at}"));
        }

        // An empty string is forwarded so the CLI rejects it as a malformed
        // declaration instead of the SDK silently disarming the guard the
        // caller believes is armed.
        if let Some(resume_drops_turn) = &self.options.resume_drops_turn {
            Self::reject_windows_cmd_metacharacters("resume_drops_turn", resume_drops_turn)?;
            cmd.push(format!("--resume-drops-turn={resume_drops_turn}"));
        }

        if self.options.session_store.is_some() {
            cmd.push("--session-mirror".into());
        }

        // Agents are always sent via the initialize request (matching the
        // TypeScript SDK); no --agents CLI flag needed.

        if let Some(sources) = effective_setting_sources {
            cmd.push(format!("--setting-sources={}", sources.join(",")));
        }

        for plugin in &self.options.plugins {
            let crate::types::SdkPluginConfig::Local { path } = plugin;
            cmd.push("--plugin-dir".into());
            cmd.push(path.clone());
        }

        // Extra args for future CLI flags.
        for (flag, value) in &self.options.extra_args {
            match value {
                None => cmd.push(format!("--{flag}")),
                Some(value) if value.starts_with('-') => {
                    // In the two-token form, a dash-leading value is not
                    // bound to its flag when the CLI declares the option with
                    // an optional value — it parses as a separate flag
                    // instead. The equals form always binds.
                    cmd.push(format!("--{flag}={value}"));
                }
                Some(value) => {
                    cmd.push(format!("--{flag}"));
                    cmd.push(value.clone());
                }
            }
        }

        // Resolve thinking config → --thinking / --max-thinking-tokens.
        // `thinking` takes precedence over the deprecated
        // `max_thinking_tokens`.
        if let Some(thinking) = &self.options.thinking {
            match thinking {
                ThinkingConfig::Adaptive { display } => {
                    cmd.push("--thinking".into());
                    cmd.push("adaptive".into());
                    if let Some(display) = display {
                        cmd.push("--thinking-display".into());
                        cmd.push(display.as_str().to_string());
                    }
                }
                ThinkingConfig::Enabled {
                    budget_tokens,
                    display,
                } => {
                    cmd.push("--max-thinking-tokens".into());
                    cmd.push(budget_tokens.to_string());
                    if let Some(display) = display {
                        cmd.push("--thinking-display".into());
                        cmd.push(display.as_str().to_string());
                    }
                }
                ThinkingConfig::Disabled => {
                    cmd.push("--thinking".into());
                    cmd.push("disabled".into());
                }
            }
        } else if let Some(max_thinking_tokens) = self.options.max_thinking_tokens {
            cmd.push("--max-thinking-tokens".into());
            cmd.push(max_thinking_tokens.to_string());
        }

        if let Some(effort) = self.options.effort {
            cmd.push("--effort".into());
            cmd.push(effort.as_str().to_string());
        }

        // Extract schema from the output_format structure if provided.
        // Expected: {"type": "json_schema", "schema": {...}}
        if let Some(output_format) = &self.options.output_format {
            if output_format.get("type").and_then(Value::as_str) == Some("json_schema") {
                if let Some(schema) = output_format.get("schema") {
                    if !schema.is_null() {
                        cmd.push("--json-schema".into());
                        cmd.push(schema.to_string());
                    }
                }
            }
        }

        // Always use streaming mode with stdin (matching the TypeScript
        // SDK). This allows agents and other large configs to be sent via
        // the initialize request.
        cmd.push("--input-format".into());
        cmd.push("stream-json".into());

        Ok(cmd)
    }

    /// Check the Claude Code version and warn if below minimum. Best-effort:
    /// failures and timeouts are ignored.
    async fn check_claude_version(cli_path: &Path) {
        let probe = async {
            let output = Command::new(cli_path)
                .arg("-v")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .ok()?;
            let version_output = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let mut parts = version_output
                .split(|c: char| !c.is_ascii_digit())
                .take(3)
                .filter_map(|p| p.parse::<u32>().ok());
            let version = [parts.next()?, parts.next()?, parts.next()?];
            Some((version_output, version))
        };
        if let Ok(Some((version_output, version))) =
            tokio::time::timeout(Duration::from_secs(2), probe).await
        {
            if version < MINIMUM_CLAUDE_CODE_VERSION {
                tracing::warn!(
                    target: "claude_agent_sdk",
                    "Claude Code version {} at {} is unsupported in the Agent SDK. \
                     Minimum required version is {}.{}.{}. Some features may not work correctly.",
                    version_output,
                    cli_path.display(),
                    MINIMUM_CLAUDE_CODE_VERSION[0],
                    MINIMUM_CLAUDE_CODE_VERSION[1],
                    MINIMUM_CLAUDE_CODE_VERSION[2],
                );
            }
        }
    }

    async fn handle_stderr(
        mut stderr: tokio::process::ChildStderr,
        callback: crate::types::StderrCallback,
        max_buffer_size: usize,
    ) {
        // `options.stderr` is documented to receive lines, but the stream
        // yields chunks, so frame the lines here rather than handing the
        // callback whatever a read happened to return.
        let mut framer = LineFramer::new();
        let mut buf = vec![0u8; 8192];
        let emit = |line: String| {
            let line = line.trim_end().to_string();
            if !line.is_empty() {
                callback(&line);
            }
        };
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for line in framer.push(&buf[..n]) {
                        emit(line);
                    }
                    // A producer that never emits a newline can't grow the
                    // buffer without bound; flush it as a partial line
                    // instead.
                    if framer.pending_len() > max_buffer_size {
                        emit(framer.flush());
                    }
                }
            }
        }
        // The last partial line still reaches the callback: a diagnostic
        // written without a trailing newline before the CLI stalled is
        // exactly what the caller needs at that moment.
        emit(framer.flush());
    }
}

/// Resolve a user name or numeric uid string to a uid.
///
/// Mirrors the name-service lookup the Python SDK gets from its process
/// layer when `ClaudeAgentOptions.user` is set. Returns `None` for unknown
/// users.
#[cfg(unix)]
fn resolve_uid(user: &str) -> Option<u32> {
    if let Ok(uid) = user.parse::<u32>() {
        return Some(uid);
    }
    let name = std::ffi::CString::new(user).ok()?;
    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0u8; 4096];
    loop {
        let ret = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut passwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        if ret == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if ret != 0 || result.is_null() {
            return None;
        }
        return Some(unsafe { (*result).pw_uid });
    }
}

/// Reject skill names that cannot ride safely in a `Skill(name)` rule.
///
/// Names from `options.skills` are formatted into the `--allowedTools` value,
/// which the CLI splits into rules on commas and spaces outside parentheses.
/// That tokenizer does not honor escape sequences, so a name carrying a
/// delimiter cannot be passed through reliably. Names that tokenize cleanly
/// but can never match the listed skill are rejected too, so a dead rule
/// fails loudly here instead of silently granting nothing.
fn validate_skill_name(name: &str) -> Result<()> {
    let invalid = |message: String| Err(ClaudeSdkError::InvalidConfig(message));
    if name.trim().is_empty() {
        return invalid("Skill names must be non-empty strings".to_string());
    }
    if name != name.trim() {
        return invalid(format!(
            "Invalid skill name {name:?}: leading or trailing whitespace can never match — the \
             Skill tool trims the invoked name."
        ));
    }
    // Parentheses and commas are delimiters to the --allowedTools tokenizer;
    // control characters (C0, DEL, C1) never appear in a skill directory
    // name. U+FEFF is included because the CLI trims it as whitespace.
    let has_invalid_char = name.chars().any(|c| {
        matches!(c, '(' | ')' | ',' | '\u{feff}')
            || ('\u{0000}'..='\u{001f}').contains(&c)
            || ('\u{007f}'..='\u{009f}').contains(&c)
    });
    if has_invalid_char {
        return invalid(format!(
            "Invalid skill name {name:?}: parentheses, commas, control characters, and \
             byte-order marks are not allowed. Names match the skill's directory name, or \
             'plugin:skill' for plugin-qualified skills."
        ));
    }
    if name == "*" {
        return invalid(
            "Invalid skill name '*': use SkillsConfig::All to enable every skill.".to_string(),
        );
    }
    if name.ends_with(":*") || name.ends_with(" *") {
        return invalid(format!(
            "Invalid skill name {name:?}: wildcard-suffix names are not allowed; list each skill \
             by its exact name."
        ));
    }
    if name.starts_with('/') {
        return invalid(format!(
            "Invalid skill name {name:?}: skill names may not start with '/'. The skills option \
             takes the canonical name, not the slash-command form."
        ));
    }
    if name.contains("\\\\") {
        return invalid(format!(
            "Invalid skill name {name:?}: consecutive backslashes are not allowed — the per-rule \
             parser collapses them, so the rule would name a different skill."
        ));
    }
    if name.ends_with('\\') {
        return invalid(format!(
            "Invalid skill name {name:?}: names may not end with an unpaired backslash."
        ));
    }
    Ok(())
}

#[async_trait]
impl Transport for SubprocessCliTransport {
    async fn connect(&self) -> Result<()> {
        if self.child.lock().await.is_some() {
            return Ok(());
        }

        let cli_path = {
            let mut guard = self.cli_path.lock().await;
            if guard.is_none() {
                *guard = Some(Self::find_cli()?);
            }
            guard.clone().expect("cli_path was just resolved")
        };

        // Validate the resolved CLI before anything is spawned with it —
        // this guards the version probe below as well as the main spawn.
        Self::reject_windows_batch_cli(&cli_path)?;

        if std::env::var("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK")
            .unwrap_or_default()
            .is_empty()
        {
            Self::check_claude_version(&cli_path).await;
        }

        let cmd = self.build_command(&cli_path)?;

        let mut command = Command::new(&cmd[0]);
        command.args(&cmd[1..]);

        // Merge environment variables. CLAUDE_CODE_ENTRYPOINT defaults to
        // sdk-rust regardless of inherited process env; options.env can
        // override it. CLAUDE_AGENT_SDK_VERSION is always set by the SDK.
        // CLAUDECODE is filtered out so SDK-spawned subprocesses don't think
        // they're running inside a Claude Code parent.
        command.env_remove("CLAUDECODE");
        command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-rust");
        for (key, value) in &self.options.env {
            command.env(key, value);
        }
        command.env("CLAUDE_AGENT_SDK_VERSION", SDK_VERSION);

        if self.options.enable_file_checkpointing {
            command.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
        }

        if let Some(cwd) = &self.cwd {
            command.env("PWD", cwd);
            command.current_dir(cwd);
        }

        if let Some(user) = &self.options.user {
            #[cfg(unix)]
            {
                // The Python SDK passes a username or uid through to the
                // spawn; accept both here (usernames resolve via getpwnam_r,
                // as the Python process layer does).
                match resolve_uid(user) {
                    Some(uid) => {
                        std::os::unix::process::CommandExt::uid(command.as_std_mut(), uid);
                    }
                    None => {
                        return Err(ClaudeSdkError::cli_connection(format!(
                            "Failed to start Claude Code: unknown user {user:?}"
                        )));
                    }
                }
            }
            #[cfg(not(unix))]
            {
                tracing::warn!(
                    target: "claude_agent_sdk",
                    "ClaudeAgentOptions.user {user:?} is not supported on this platform; ignoring"
                );
            }
        }

        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        // Pipe stderr only when the caller registered a callback.
        let stderr_piped = self.options.stderr.is_some();
        command.stderr(if stderr_piped {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        // Backstop against orphaned `claude` processes when callers drop the
        // transport without awaiting close() (the atexit reaper in the
        // Python SDK).
        command.kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Check if the error comes from the working directory or the
                // CLI.
                if let Some(cwd) = &self.cwd {
                    if !cwd.exists() {
                        return Err(ClaudeSdkError::cli_connection(format!(
                            "Working directory does not exist: {}",
                            cwd.display()
                        )));
                    }
                }
                return Err(ClaudeSdkError::cli_not_found(
                    format!("Claude Code not found at: {}", cli_path.display()),
                    None,
                ));
            }
            Err(e) => {
                return Err(ClaudeSdkError::cli_connection(format!(
                    "Failed to start Claude Code: {e}"
                )));
            }
        };

        *self.stdout.lock().await = child.stdout.take();
        if let (true, Some(stderr), Some(callback)) = (
            stderr_piped,
            child.stderr.take(),
            self.options.stderr.clone(),
        ) {
            let max_buffer_size = self.max_buffer_size;
            *self.stderr_task.lock().await = Some(tokio::spawn(Self::handle_stderr(
                stderr,
                callback,
                max_buffer_size,
            )));
        }
        {
            let mut stdin_state = self.stdin.lock().await;
            stdin_state.stdin = child.stdin.take();
            stdin_state.ready = true;
            stdin_state.exit_error = None;
        }
        *self.child.lock().await = Some(child);
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn write(&self, data: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        // All checks inside the lock to prevent TOCTOU races with
        // close()/end_input().
        let mut state = self.stdin.lock().await;
        if !state.ready || state.stdin.is_none() {
            return Err(ClaudeSdkError::cli_connection(
                "ProcessTransport is not ready for writing",
            ));
        }
        if let Some(exit_error) = &state.exit_error {
            return Err(ClaudeSdkError::cli_connection(format!(
                "Cannot write to process that exited with error: {exit_error}"
            )));
        }
        if let Some(child) = self.child.lock().await.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(ClaudeSdkError::cli_connection(format!(
                    "Cannot write to terminated process (exit code: {})",
                    status.code().unwrap_or(-1)
                )));
            }
        }
        let stdin = state.stdin.as_mut().expect("checked above");
        let write_result = async {
            stdin.write_all(data.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        if let Err(e) = write_result {
            state.ready = false;
            let message = format!("Failed to write to process stdin: {e}");
            state.exit_error = Some(message.clone());
            return Err(ClaudeSdkError::cli_connection(message));
        }
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, Result<Value>> {
        struct ReadState {
            stdout: Option<ChildStdout>,
            child: Arc<Mutex<Option<Child>>>,
            framer: LineFramer,
            pending: VecDeque<Result<Value>>,
            buf: Vec<u8>,
            max_buffer_size: usize,
            eof: bool,
            done: bool,
        }

        // The stream owns stdout; taking it here (blocking_lock is safe: no
        // await points hold this mutex) means read_messages can only be
        // consumed once per connection, matching how the SDK uses it.
        let stdout = self.stdout.try_lock().ok().and_then(|mut s| s.take());
        let state = ReadState {
            stdout,
            child: self.child.clone(),
            framer: LineFramer::new(),
            pending: VecDeque::new(),
            buf: vec![0u8; 65536],
            max_buffer_size: self.max_buffer_size,
            eof: false,
            done: false,
        };

        fn guard(length: usize, max: usize) -> Result<()> {
            // Bound a single message, whether it is complete yet or not.
            if length > max {
                return Err(ClaudeSdkError::JsonDecode {
                    line: format!("JSON message exceeded maximum buffer size of {max} bytes"),
                    original_error: format!("Buffer size {length} exceeds limit {max}"),
                });
            }
            Ok(())
        }

        futures::stream::unfold(state, |mut state| async move {
            loop {
                if state.done {
                    return None;
                }
                if let Some(item) = state.pending.pop_front() {
                    // An error ends the stream after being yielded.
                    if item.is_err() {
                        state.done = true;
                    }
                    return Some((item, state));
                }
                if state.eof {
                    // Flush whatever is left. The CLI terminates every
                    // message with "\n", so a residual tail means either a
                    // producer that omits the final newline (yield it) or
                    // one cut off mid-write (unrecoverable — drop it).
                    let tail = state.framer.flush();
                    match parse_stdout_line(&tail) {
                        Ok(Some(data)) => state.pending.push_back(Ok(data)),
                        Ok(None) => {}
                        Err(_) => {
                            tracing::debug!(
                                target: "claude_agent_sdk",
                                "Dropping truncated JSON at end of CLI stdout: {}",
                                &tail[..tail.len().min(200)]
                            );
                        }
                    }
                    // Check process completion and surface a non-zero exit as
                    // a ProcessError after the buffered messages drain.
                    let returncode = {
                        let mut child_guard = state.child.lock().await;
                        match child_guard.as_mut() {
                            Some(child) => match child.wait().await {
                                Ok(status) => status.code().unwrap_or(-1),
                                Err(_) => -1,
                            },
                            None => 0,
                        }
                    };
                    if returncode != 0 {
                        state.pending.push_back(Err(ClaudeSdkError::Process {
                            message: format!("Command failed with exit code {returncode}"),
                            exit_code: Some(returncode),
                            stderr: Some("Check stderr output for details".to_string()),
                        }));
                    }
                    state.eof = false;
                    state.done = state.pending.is_empty();
                    if state.done {
                        return None;
                    }
                    continue;
                }
                let Some(stdout) = state.stdout.as_mut() else {
                    state.done = true;
                    return Some((Err(ClaudeSdkError::cli_connection("Not connected")), state));
                };
                match stdout.read(&mut state.buf).await {
                    Ok(0) => {
                        state.eof = true;
                        state.stdout = None;
                    }
                    Err(e) => {
                        // A real read failure is not EOF: surface it rather
                        // than silently ending the stream (matching the
                        // Python transport, which only suppresses
                        // closed-resource errors).
                        state.stdout = None;
                        state
                            .pending
                            .push_back(Err(ClaudeSdkError::cli_connection(format!(
                                "Failed to read from CLI stdout: {e}"
                            ))));
                    }
                    Ok(n) => {
                        let lines = state.framer.push(&state.buf[..n]);
                        for line in lines {
                            if let Err(e) = guard(line.len(), state.max_buffer_size) {
                                state.pending.push_back(Err(e));
                                break;
                            }
                            match parse_stdout_line(&line) {
                                Ok(Some(data)) => state.pending.push_back(Ok(data)),
                                Ok(None) => {}
                                Err(e) => {
                                    state.pending.push_back(Err(e));
                                    break;
                                }
                            }
                        }
                        if let Err(e) = guard(state.framer.pending_len(), state.max_buffer_size) {
                            state.pending.push_back(Err(e));
                        }
                    }
                }
            }
        })
        .boxed()
    }

    async fn close(&self) -> Result<()> {
        if self.child.lock().await.is_none() {
            self.ready.store(false, Ordering::SeqCst);
            return Ok(());
        }

        // Cancel the stderr reader if active.
        if let Some(task) = self.stderr_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }

        // Close stdin (holding the write lock to prevent a race with
        // concurrent writes). Bounded: a writer blocked on a full stdin pipe
        // must not pin close() forever.
        if let Ok(lock_result) =
            tokio::time::timeout(Duration::from_secs(5), self.stdin.lock()).await
        {
            let mut state = lock_result;
            state.ready = false;
            state.stdin.take(); // dropping ChildStdin closes the pipe
        }
        self.ready.store(false, Ordering::SeqCst);

        // Wait for graceful shutdown after stdin EOF, then terminate if
        // needed. The subprocess needs time to flush its session file after
        // receiving EOF on stdin; without this grace period, killing can
        // interrupt the write and lose the last assistant message.
        let mut child_guard = self.child.lock().await;
        if let Some(child) = child_guard.as_mut() {
            let already_exited = matches!(child.try_wait(), Ok(Some(_)));
            if !already_exited {
                let graceful = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                if graceful.is_err() {
                    // Graceful shutdown timed out — terminate (SIGTERM on
                    // Unix; hard kill elsewhere).
                    #[cfg(unix)]
                    {
                        if let Some(pid) = child.id() {
                            unsafe {
                                libc::kill(pid as libc::pid_t, libc::SIGTERM);
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.start_kill();
                    }
                    let terminated =
                        tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    if terminated.is_err() {
                        // SIGTERM handler blocked — force kill (SIGKILL).
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    }
                }
            }
        }
        // Drop the child handle; if the process somehow survived the
        // escalation above, kill_on_drop delivers the final SIGKILL.
        *child_guard = None;
        drop(child_guard);
        *self.stdout.lock().await = None;
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    async fn end_input(&self) -> Result<()> {
        let mut state = self.stdin.lock().await;
        state.stdin.take(); // dropping ChildStdin closes the pipe
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_framer_reassembles_split_lines() {
        let mut framer = LineFramer::new();
        assert!(framer.push(b"{\"a\": ").is_empty());
        let lines = framer.push(b"1}\n{\"b\": 2}\n{\"c\":");
        assert_eq!(lines, vec!["{\"a\": 1}", "{\"b\": 2}"]);
        assert_eq!(framer.flush(), "{\"c\":");
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn line_framer_preserves_utf8_split_across_chunks() {
        // A multi-byte character split across two reads must survive intact:
        // framing happens on bytes, and decoding only on complete lines.
        let text = "{\"text\": \"héllo wörld ✓\"}\n";
        let bytes = text.as_bytes();
        // Split inside the 'é' (0xC3 0xA9) and inside '✓' (3 bytes).
        for split in 1..bytes.len() {
            let mut framer = LineFramer::new();
            let mut lines = framer.push(&bytes[..split]);
            lines.extend(framer.push(&bytes[split..]));
            assert_eq!(lines, vec![text.trim_end().to_string()], "split at {split}");
        }
    }

    #[test]
    fn parse_stdout_line_skips_non_json() {
        assert!(parse_stdout_line("").unwrap().is_none());
        assert!(parse_stdout_line("[SandboxDebug] hi").unwrap().is_none());
        assert!(parse_stdout_line("{\"a\": 1}").unwrap().is_some());
        assert!(parse_stdout_line("{broken").is_err());
    }

    fn command_for(options: ClaudeAgentOptions) -> Vec<String> {
        SubprocessCliTransport::new(options)
            .build_command(Path::new("/usr/bin/claude"))
            .expect("build_command")
    }

    fn flag_value(cmd: &[String], flag: &str) -> Option<String> {
        cmd.iter()
            .position(|a| a == flag)
            .and_then(|i| cmd.get(i + 1))
            .cloned()
    }

    #[test]
    fn build_command_defaults() {
        let cmd = command_for(ClaudeAgentOptions::default());
        assert_eq!(cmd[0], "/usr/bin/claude");
        assert_eq!(
            flag_value(&cmd, "--output-format").as_deref(),
            Some("stream-json")
        );
        assert_eq!(
            flag_value(&cmd, "--input-format").as_deref(),
            Some("stream-json")
        );
        assert!(cmd.contains(&"--verbose".to_string()));
        // A None system prompt is sent as an explicit empty string.
        assert_eq!(flag_value(&cmd, "--system-prompt").as_deref(), Some(""));
    }

    #[test]
    fn build_command_maps_options_to_flags() {
        let options = ClaudeAgentOptions {
            system_prompt: Some(SystemPrompt::Preset {
                append: Some("Be brief.".to_string()),
                exclude_dynamic_sections: Some(true),
            }),
            tools: Some(ToolsConfig::Preset),
            allowed_tools: vec!["Read".to_string(), "Bash(ls:*)".to_string()],
            disallowed_tools: vec!["WebFetch".to_string()],
            max_turns: Some(5),
            max_budget_usd: Some(1.5),
            model: Some("claude-sonnet-4-5".to_string()),
            fallback_model: Some("claude-opus-4-1".to_string()),
            permission_mode: Some(crate::types::PermissionMode::AcceptEdits),
            continue_conversation: true,
            include_partial_messages: true,
            strict_mcp_config: true,
            fork_session: true,
            effort: Some(crate::types::EffortLevel::Xhigh),
            thinking: Some(ThinkingConfig::Adaptive {
                display: Some(crate::types::ThinkingDisplay::Summarized),
            }),
            task_budget: Some(crate::types::TaskBudget { total: 64_000 }),
            extra_args: [
                ("bool-flag".to_string(), None),
                ("dash-value".to_string(), Some("-x".to_string())),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let cmd = command_for(options);
        assert_eq!(
            flag_value(&cmd, "--append-system-prompt").as_deref(),
            Some("Be brief.")
        );
        assert_eq!(flag_value(&cmd, "--tools").as_deref(), Some("default"));
        assert_eq!(
            flag_value(&cmd, "--allowedTools").as_deref(),
            Some("Read,Bash(ls:*)")
        );
        assert_eq!(
            flag_value(&cmd, "--disallowedTools").as_deref(),
            Some("WebFetch")
        );
        assert_eq!(flag_value(&cmd, "--max-turns").as_deref(), Some("5"));
        assert_eq!(flag_value(&cmd, "--max-budget-usd").as_deref(), Some("1.5"));
        assert_eq!(
            flag_value(&cmd, "--model").as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            flag_value(&cmd, "--fallback-model").as_deref(),
            Some("claude-opus-4-1")
        );
        assert_eq!(
            flag_value(&cmd, "--permission-mode").as_deref(),
            Some("acceptEdits")
        );
        assert!(cmd.contains(&"--continue".to_string()));
        assert!(cmd.contains(&"--include-partial-messages".to_string()));
        assert!(cmd.contains(&"--strict-mcp-config".to_string()));
        assert!(cmd.contains(&"--fork-session".to_string()));
        assert_eq!(flag_value(&cmd, "--effort").as_deref(), Some("xhigh"));
        assert_eq!(flag_value(&cmd, "--thinking").as_deref(), Some("adaptive"));
        assert_eq!(
            flag_value(&cmd, "--thinking-display").as_deref(),
            Some("summarized")
        );
        assert_eq!(flag_value(&cmd, "--task-budget").as_deref(), Some("64000"));
        assert!(cmd.contains(&"--bool-flag".to_string()));
        // Dash-leading extra-arg values bind with '=' so they can't parse as
        // a separate flag.
        assert!(cmd.contains(&"--dash-value=-x".to_string()));
    }

    #[test]
    fn build_command_resume_uses_equals_form() {
        let options = ClaudeAgentOptions {
            resume: Some("abc-123".to_string()),
            session_id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            resume_session_at: Some("uuid-1".to_string()),
            resume_drops_turn: Some(String::new()),
            ..Default::default()
        };
        let cmd = command_for(options);
        assert!(cmd.contains(&"--resume=abc-123".to_string()));
        assert!(cmd.contains(&"--session-id=550e8400-e29b-41d4-a716-446655440000".to_string()));
        assert!(cmd.contains(&"--resume-session-at=uuid-1".to_string()));
        // An empty resume_drops_turn is forwarded so the CLI rejects it
        // rather than the SDK silently disarming the guard.
        assert!(cmd.contains(&"--resume-drops-turn=".to_string()));
    }

    #[test]
    fn build_command_strips_sdk_mcp_instances() {
        use crate::mcp::create_sdk_mcp_server;
        let sdk = create_sdk_mcp_server("calc", "1.0.0", vec![]);
        let options = ClaudeAgentOptions {
            mcp_servers: McpServers::Map(
                [
                    ("calc".to_string(), crate::types::McpServerConfig::Sdk(sdk)),
                    (
                        "ext".to_string(),
                        crate::types::McpServerConfig::Stdio(crate::types::McpStdioServerConfig {
                            command: "server".to_string(),
                            args: vec!["--port".to_string()],
                            env: Default::default(),
                        }),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };
        let cmd = command_for(options);
        let config: Value =
            serde_json::from_str(&flag_value(&cmd, "--mcp-config").expect("--mcp-config")).unwrap();
        assert_eq!(
            config["mcpServers"]["calc"],
            json!({"type": "sdk", "name": "calc"})
        );
        assert_eq!(config["mcpServers"]["ext"]["type"], "stdio");
        assert_eq!(config["mcpServers"]["ext"]["command"], "server");
        assert!(config["mcpServers"]["calc"].get("instance").is_none());
    }

    #[test]
    fn build_command_skills_defaults() {
        // SkillsConfig::List injects Skill(name) rules and defaults
        // setting_sources to user,project.
        let options = ClaudeAgentOptions {
            skills: Some(SkillsConfig::List(vec!["writer".to_string()])),
            ..Default::default()
        };
        let cmd = command_for(options);
        assert_eq!(
            flag_value(&cmd, "--allowedTools").as_deref(),
            Some("Skill(writer)")
        );
        assert!(cmd.contains(&"--setting-sources=user,project".to_string()));

        // SkillsConfig::All injects a bare Skill rule.
        let options = ClaudeAgentOptions {
            skills: Some(SkillsConfig::All),
            setting_sources: Some(vec![crate::types::SettingSource::Project]),
            ..Default::default()
        };
        let cmd = command_for(options);
        assert_eq!(flag_value(&cmd, "--allowedTools").as_deref(), Some("Skill"));
        assert!(cmd.contains(&"--setting-sources=project".to_string()));

        // Invalid names fail at command build.
        let options = ClaudeAgentOptions {
            skills: Some(SkillsConfig::List(vec!["a,b".to_string()])),
            ..Default::default()
        };
        assert!(SubprocessCliTransport::new(options)
            .build_command(Path::new("/usr/bin/claude"))
            .is_err());
    }

    #[test]
    fn build_command_merges_sandbox_into_settings() {
        let options = ClaudeAgentOptions {
            settings: Some(r#"{"model": "opus"}"#.to_string()),
            sandbox: Some(crate::types::SandboxSettings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cmd = command_for(options);
        let settings: Value =
            serde_json::from_str(&flag_value(&cmd, "--settings").expect("--settings")).unwrap();
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["sandbox"]["enabled"], true);
    }

    #[test]
    fn build_command_output_format_schema() {
        let options = ClaudeAgentOptions {
            output_format: Some(json!({
                "type": "json_schema",
                "schema": {"type": "object", "properties": {"answer": {"type": "string"}}},
            })),
            ..Default::default()
        };
        let cmd = command_for(options);
        let schema: Value =
            serde_json::from_str(&flag_value(&cmd, "--json-schema").expect("--json-schema"))
                .unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn validate_skill_name_rejects_delimiters() {
        assert!(validate_skill_name("my-skill").is_ok());
        assert!(validate_skill_name("plugin:skill").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name(" padded ").is_err());
        assert!(validate_skill_name("a,b").is_err());
        assert!(validate_skill_name("a(b)").is_err());
        assert!(validate_skill_name("*").is_err());
        assert!(validate_skill_name("plugin:*").is_err());
        assert!(validate_skill_name("/slash").is_err());
        assert!(validate_skill_name("a\\\\b").is_err());
        assert!(validate_skill_name("trailing\\").is_err());
    }
}
