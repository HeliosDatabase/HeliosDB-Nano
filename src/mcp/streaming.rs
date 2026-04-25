//! Async wrapper around the (sync) tool dispatcher that delivers
//! [`super::progress::ProgressEvent`]s through a channel as the tool
//! runs.
//!
//! The handler stays synchronous — handlers call
//! [`super::progress::emit`] from anywhere and the thread-local sink
//! we install before invoking the handler routes events into a tokio
//! channel.  `spawn_blocking` keeps the runtime healthy even when a
//! tool blocks on RocksDB I/O.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::EmbeddedDatabase;

use super::progress::{self, ChannelProgressSink, ProgressEvent, ProgressSink};
use super::tools::{call_tool, ToolOutcome};

/// Run a tool with a thread-local progress sink installed. Returns
/// the receiver half of an unbounded channel of progress events, plus
/// a `JoinHandle` that resolves to the final tool outcome once the
/// handler returns.
///
/// Callers consume `rx` until the handle completes — by then the
/// channel is closed (the sink drops with the spawn_blocking
/// closure), so a `while let Some(ev) = rx.recv().await` loop wakes
/// up cleanly on close.
pub fn call_tool_streaming(
    db: Option<Arc<EmbeddedDatabase>>,
    name: String,
    args: JsonValue,
) -> (mpsc::UnboundedReceiver<ProgressEvent>, JoinHandle<ToolOutcome>) {
    let (sink, rx) = ChannelProgressSink::channel();
    let sink_arc: Arc<dyn ProgressSink> = Arc::new(sink);
    let handle = tokio::task::spawn_blocking(move || {
        progress::set_sink(sink_arc);
        // Sync call — `emit` calls inside the handler reach our
        // channel via the thread-local.
        let outcome = call_tool(db.as_deref(), &name, args);
        progress::clear_sink();
        outcome
    });
    (rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::progress::emit;
    use crate::mcp::tools::ToolOutcome;
    use serde_json::json;

    // Drive a fake tool that emits N progress events and then
    // returns success. Demonstrates the channel + spawn_blocking
    // integration without depending on tool internals.
    fn fake_streaming_handler(args: JsonValue) -> ToolOutcome {
        let n = args["n"].as_u64().unwrap_or(0);
        for i in 0..n {
            emit(ProgressEvent::new(i as f64).with_total(n as f64));
        }
        ToolOutcome::ok(json!({ "emitted": n }))
    }

    #[tokio::test]
    async fn channel_drains_emitted_events() {
        // Don't go through the dispatcher; we exercise the
        // thread-local + channel wiring directly so the test is
        // independent of the (heavy) tool catalogue.
        let (sink, mut rx) = ChannelProgressSink::channel();
        let sink_arc: Arc<dyn ProgressSink> = Arc::new(sink);
        let handle = tokio::task::spawn_blocking(move || {
            progress::set_sink(sink_arc);
            let outcome = fake_streaming_handler(json!({ "n": 3 }));
            progress::clear_sink();
            outcome
        });

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        let outcome = handle.await.unwrap();
        assert!(!outcome.is_error);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].progress, 0.0);
        assert_eq!(events[2].progress, 2.0);
    }
}
