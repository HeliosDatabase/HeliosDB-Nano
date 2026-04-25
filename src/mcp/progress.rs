//! `notifications/progress` plumbing for long-running MCP tools.
//!
//! The MCP spec lets clients opt into progress reports by passing
//! `_meta: { progressToken: <id> }` alongside `tools/call` params.
//! The server then emits zero or more
//! `notifications/progress` messages over the same transport while
//! the tool runs, and finally returns the regular `tools/call`
//! response.
//!
//! This module decouples *where* a tool emits progress (anywhere on
//! the call stack via [`emit`]) from *who* delivers those events to
//! the client (the transport — stdio, WebSocket, SSE).
//!
//! Wiring shape:
//!
//! 1. Transport detects `progressToken` on an incoming `tools/call`.
//! 2. Transport calls [`crate::mcp::streaming::call_tool_streaming`]
//!    instead of `call_tool`. That helper sets up a channel-backed
//!    sink, runs the (sync) handler on a `spawn_blocking` thread,
//!    and returns a receiver.
//! 3. Transport forwards each [`ProgressEvent`] off the receiver as a
//!    `notifications/progress` JSON-RPC notification, then sends the
//!    final response when the join handle completes.
//!
//! Tools that don't care about progress need no change. Tools that
//! want to emit progress call [`emit`] from anywhere on the call
//! stack — when no sink is active (the default), it's a cheap no-op.

use std::cell::RefCell;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressEvent {
    /// Monotonically-non-decreasing progress count. Spec leaves the
    /// units to the server; pair with `total` if you have one.
    pub progress: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ProgressEvent {
    pub fn new(progress: f64) -> Self {
        Self { progress, total: None, message: None }
    }

    pub fn with_total(mut self, total: f64) -> Self {
        self.total = Some(total);
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

/// Anything that can absorb [`ProgressEvent`]s. Implementations must
/// be cheap to call — handlers fire-and-forget many of these.
pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: ProgressEvent);
}

/// Default implementation: drops every event.
#[derive(Debug, Default)]
pub struct NoopProgressSink;

impl ProgressSink for NoopProgressSink {
    fn emit(&self, _event: ProgressEvent) {}
}

/// Channel-backed sink. The [`emit`] call is non-blocking; if the
/// consumer is slow events queue up in the unbounded channel.
#[derive(Debug, Clone)]
pub struct ChannelProgressSink {
    sender: mpsc::UnboundedSender<ProgressEvent>,
}

impl ChannelProgressSink {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<ProgressEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { sender: tx }, rx)
    }
}

impl ProgressSink for ChannelProgressSink {
    fn emit(&self, event: ProgressEvent) {
        // Receiver dropped means the transport gave up — silently
        // discard so handlers don't have to care.
        let _ = self.sender.send(event);
    }
}

// ---- Thread-local active sink ----------------------------------------

thread_local! {
    static ACTIVE_SINK: RefCell<Option<Arc<dyn ProgressSink>>> = const { RefCell::new(None) };
}

/// Install a progress sink for the current thread. Pair with
/// [`clear_sink`]. Designed for use from the streaming dispatcher
/// inside `spawn_blocking` — handlers don't call this directly.
pub fn set_sink(sink: Arc<dyn ProgressSink>) {
    ACTIVE_SINK.with(|s| *s.borrow_mut() = Some(sink));
}

/// Drop the thread-local sink. Always safe to call; idempotent.
pub fn clear_sink() {
    ACTIVE_SINK.with(|s| *s.borrow_mut() = None);
}

/// Emit a progress event to whatever sink is active on the current
/// thread. No-op when no sink is installed (the default).
pub fn emit(event: ProgressEvent) {
    ACTIVE_SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink.emit(event);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_without_sink_is_noop() {
        // Nothing installed — should not panic, should not block.
        emit(ProgressEvent::new(1.0));
    }

    #[test]
    fn channel_sink_collects_events() {
        let (sink, mut rx) = ChannelProgressSink::channel();
        sink.emit(ProgressEvent::new(0.0).with_total(2.0).with_message("a"));
        sink.emit(ProgressEvent::new(1.0).with_total(2.0).with_message("b"));
        sink.emit(ProgressEvent::new(2.0).with_total(2.0).with_message("c"));
        drop(sink);
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].progress, 0.0);
        assert_eq!(got[2].message.as_deref(), Some("c"));
    }

    #[test]
    fn thread_local_sink_routes_emit() {
        let (sink, mut rx) = ChannelProgressSink::channel();
        set_sink(Arc::new(sink));
        emit(ProgressEvent::new(7.0));
        clear_sink();
        // After clear, emits should be silent.
        emit(ProgressEvent::new(99.0));
        let first = rx.try_recv().expect("first event");
        assert_eq!(first.progress, 7.0);
        assert!(rx.try_recv().is_err(), "no second event after clear_sink");
    }
}
