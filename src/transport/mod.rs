//! Transport implementations for the Claude Agent SDK.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::errors::Result;

mod subprocess_cli;

pub use subprocess_cli::SubprocessCliTransport;

/// Abstract transport for Claude communication.
///
/// WARNING: This API is exposed for custom transport implementations (e.g.
/// remote Claude Code connections). It may change in any future release, and
/// custom implementations must be updated to match interface changes.
///
/// This is a low-level transport interface that handles raw I/O with the
/// Claude process or service. The SDK's query layer builds on top of this to
/// implement the control protocol and message routing.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Connect the transport and prepare for communication.
    ///
    /// For subprocess transports, this starts the process. For network
    /// transports, this establishes the connection.
    async fn connect(&self) -> Result<()>;

    /// Write raw data to the transport (typically JSON + newline).
    async fn write(&self, data: &str) -> Result<()>;

    /// Read and parse messages from the transport.
    ///
    /// Called once per connection, after [`Transport::connect`]; the
    /// returned stream yields each parsed JSON message, and ends after the
    /// underlying connection closes (yielding a final error if the transport
    /// failed).
    fn read_messages(&self) -> BoxStream<'static, Result<Value>>;

    /// Close the transport connection and clean up resources.
    ///
    /// Contract: implementations must bound their own awaits (e.g. with
    /// `tokio::time::timeout`) rather than relying on the caller to cancel
    /// them — an implementation that blocks forever here blocks the caller's
    /// `disconnect()` forever with no way out. The SDK's own
    /// [`SubprocessCliTransport::close`] bounds every await (~15s worst
    /// case).
    async fn close(&self) -> Result<()>;

    /// Whether the transport is ready to send/receive messages.
    fn is_ready(&self) -> bool;

    /// End the input stream (close stdin for process transports).
    async fn end_input(&self) -> Result<()>;
}
