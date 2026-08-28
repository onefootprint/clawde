//! Batching layer between `transcript_mirror` stdout frames and a
//! [`SessionStore`].
//!
//! The CLI subprocess emits `{"type": "transcript_mirror", "filePath": ...,
//! "entries": [...]}` frames interleaved with normal SDK messages. The
//! receive loop peels these off and hands them to
//! [`TranscriptMirrorBatcher::enqueue`], which accumulates them and flushes
//! to [`SessionStore::append`] either when a `result` message arrives
//! (explicit flush) or when the pending buffer exceeds size thresholds
//! (eager background flush). This keeps adapter latency off the hot path
//! during model streaming.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::session_store::file_path_to_session_key;
use crate::types::{SessionKey, SessionStore, SessionStoreEntry};

/// Eager-flush threshold: pending entries.
pub(crate) const MAX_PENDING_ENTRIES: usize = 500;
/// Eager-flush threshold: pending bytes.
pub(crate) const MAX_PENDING_BYTES: usize = 1 << 20; // 1 MiB
/// Timeout for a single `append` call.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounded retry for transient adapter failures. Backoff list length must be
/// `MIRROR_APPEND_MAX_ATTEMPTS - 1`.
const MIRROR_APPEND_MAX_ATTEMPTS: usize = 3;
const MIRROR_APPEND_BACKOFF: [Duration; 2] =
    [Duration::from_millis(200), Duration::from_millis(800)];

/// Callback invoked when a batch is dropped after exhausting retries.
pub(crate) type MirrorErrorCallback =
    Arc<dyn Fn(Option<SessionKey>, String) -> BoxFuture<'static, ()> + Send + Sync>;

struct MirrorEntry {
    file_path: String,
    entries: Vec<Value>,
    bytes: usize,
}

struct Pending {
    items: Vec<MirrorEntry>,
    entries: usize,
    bytes: usize,
}

/// Accumulates `transcript_mirror` frames and flushes them to a store.
///
/// `enqueue` is fire-and-forget; `flush` is async. The pending queue is
/// bounded — when it exceeds the thresholds an eager flush fires in the
/// background so memory stays flat during long turns where no `result` (and
/// thus no explicit `flush()`) arrives.
///
/// Adapter failures are retried (3 attempts total) with short backoff;
/// timeouts are not retried since the in-flight call may still land. Only
/// after the final attempt fails is the batch dropped and reported via
/// `on_error`. Failures never propagate — the local-disk transcript is
/// already durable so the session must continue unaffected. Adapters should
/// dedupe by `entry["uuid"]` when present since a retried batch may
/// partially overlap a prior partial write.
pub(crate) struct TranscriptMirrorBatcher {
    store: Arc<dyn SessionStore>,
    projects_dir: String,
    on_error: MirrorErrorCallback,
    send_timeout: Duration,
    max_pending_entries: usize,
    max_pending_bytes: usize,
    pending: std::sync::Mutex<Pending>,
    // Serializes flushes so append ordering holds across eager and explicit
    // flushes.
    flush_lock: Mutex<()>,
}

impl TranscriptMirrorBatcher {
    pub(crate) fn new(
        store: Arc<dyn SessionStore>,
        projects_dir: String,
        on_error: MirrorErrorCallback,
        max_pending_entries: usize,
        max_pending_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            projects_dir,
            on_error,
            send_timeout: SEND_TIMEOUT,
            max_pending_entries,
            max_pending_bytes,
            pending: std::sync::Mutex::new(Pending {
                items: Vec::new(),
                entries: 0,
                bytes: 0,
            }),
            flush_lock: Mutex::new(()),
        })
    }

    /// Buffer a frame; schedule an eager flush if thresholds are exceeded.
    pub(crate) fn enqueue(self: &Arc<Self>, file_path: String, entries: Vec<Value>) {
        // Approximate wire size — one stringify per frame (not per entry)
        // keeps this cheap relative to the parse the transport already did.
        let size = serde_json::to_string(&entries)
            .map(|s| s.len())
            .unwrap_or(0);
        let over_threshold = {
            let mut pending = self.pending.lock().expect("mirror batcher lock poisoned");
            pending.entries += entries.len();
            pending.bytes += size;
            pending.items.push(MirrorEntry {
                file_path,
                entries,
                bytes: size,
            });
            pending.entries > self.max_pending_entries || pending.bytes > self.max_pending_bytes
        };
        if over_threshold {
            // Fire-and-forget; the flush lock serializes against any
            // in-flight flush so append ordering holds.
            let this = self.clone();
            tokio::spawn(async move { this.drain().await });
        }
    }

    /// Flush all pending entries, serialized after any in-flight eager flush.
    pub(crate) async fn flush(&self) {
        self.drain().await;
    }

    /// Final flush before teardown. Never fails.
    pub(crate) async fn close(&self) {
        self.flush().await;
    }

    /// Detach the pending buffer, await any prior flush, then send.
    ///
    /// Detaching happens before acquiring the lock so `enqueue` can keep
    /// accumulating into a fresh buffer while a prior flush is in flight.
    /// Never fails — adapter and `on_error` callback errors are logged.
    async fn drain(&self) {
        let items = {
            let mut pending = self.pending.lock().expect("mirror batcher lock poisoned");
            pending.entries = 0;
            pending.bytes = 0;
            std::mem::take(&mut pending.items)
        };
        let mut errors: Vec<(SessionKey, String)> = Vec::new();
        {
            let _guard = self.flush_lock.lock().await;
            if items.is_empty() {
                return;
            }
            self.do_flush(items, &mut errors).await;
        }
        // Report errors after releasing the lock so a slow on_error callback
        // cannot block subsequent drains (which only need the lock for
        // append-ordering).
        for (key, msg) in errors {
            (self.on_error)(Some(key), msg).await;
        }
    }

    async fn do_flush(&self, items: Vec<MirrorEntry>, errors: &mut Vec<(SessionKey, String)>) {
        // Coalesce by file_path so each unique file gets one append per flush
        // instead of one per enqueued frame. First-seen order is preserved;
        // entries within a path keep enqueue order.
        let mut order: Vec<String> = Vec::new();
        let mut by_path: std::collections::HashMap<String, Vec<Value>> =
            std::collections::HashMap::new();
        for item in items {
            let bucket = by_path.entry(item.file_path.clone()).or_insert_with(|| {
                order.push(item.file_path.clone());
                Vec::new()
            });
            let _ = item.bytes;
            bucket.extend(item.entries);
        }

        for file_path in order {
            let Some(entries) = by_path.remove(&file_path) else {
                continue;
            };
            if entries.is_empty() {
                // Avoid creating phantom keys in adapters that touch storage
                // on append([]) — nothing to write.
                continue;
            }
            let Some(key) = file_path_to_session_key(&file_path, &self.projects_dir) else {
                tracing::warn!(
                    target: "clawde",
                    "[SessionStore] dropping mirror frame: filePath {file_path} is not under {} \
                     -- subprocess CLAUDE_CONFIG_DIR likely differs from parent (custom env / \
                     container?)",
                    self.projects_dir
                );
                continue;
            };
            let typed_entries: Vec<SessionStoreEntry> = entries
                .into_iter()
                .filter_map(|e| match e {
                    Value::Object(map) => Some(map),
                    _ => None,
                })
                .collect();

            let mut last_err: Option<String> = None;
            let mut succeeded = false;
            for attempt in 0..MIRROR_APPEND_MAX_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(MIRROR_APPEND_BACKOFF[attempt - 1]).await;
                }
                match tokio::time::timeout(
                    self.send_timeout,
                    self.store.append(&key, typed_entries.clone()),
                )
                .await
                {
                    Ok(Ok(())) => {
                        succeeded = true;
                        break;
                    }
                    Ok(Err(e)) => {
                        last_err = Some(e.to_string());
                        tracing::debug!(
                            target: "clawde",
                            "[TranscriptMirrorBatcher] append attempt {}/{} failed for {}: {e}",
                            attempt + 1,
                            MIRROR_APPEND_MAX_ATTEMPTS,
                            file_path,
                        );
                    }
                    Err(_) => {
                        // Don't retry on timeout: cancellation is best-effort
                        // for adapters wrapping non-cancellable I/O, so the
                        // in-flight call may still land — a retry would
                        // launch a concurrent duplicate. Also keeps the
                        // worst-case lock hold at ~send_timeout.
                        last_err = Some(format!(
                            "append timed out after {:.1}s",
                            self.send_timeout.as_secs_f64()
                        ));
                        tracing::debug!(
                            target: "clawde",
                            "[TranscriptMirrorBatcher] append timed out after {:.1}s for {} — \
                             not retrying",
                            self.send_timeout.as_secs_f64(),
                            file_path,
                        );
                        break;
                    }
                }
            }
            if !succeeded {
                let err = last_err.unwrap_or_else(|| "unknown error".to_string());
                tracing::error!(
                    target: "clawde",
                    "[TranscriptMirrorBatcher] flush failed for {file_path}: {err}"
                );
                errors.push((key, err));
            }
        }
    }
}
