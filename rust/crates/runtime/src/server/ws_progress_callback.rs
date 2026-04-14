//! WebSocket-based tool progress callback.
//!
//! Implements [`ToolProgressCallback`] by forwarding events through an `mpsc`
//! channel that the WS handler drains during its polling loop.  Unlike the
//! approval gate, progress events are fire-and-forget — no response is needed.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use astra_tools::ToolProgressCallback;

/// A progress event to be forwarded over WebSocket.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum ProgressEvent {
    #[serde(rename = "started")]
    Started {
        call_id: String,
        tool: String,
        args: Value,
    },
    #[serde(rename = "delta")]
    Delta { call_id: String, content: String },
    #[serde(rename = "completed")]
    Completed { call_id: String, success: bool },
}

/// [`ToolProgressCallback`] implementation backed by an `mpsc` channel.
///
/// The WS handler drains the receiver side and emits
/// `ToolExecutionStarted` / `ToolOutputDelta` / `ToolExecutionCompleted`
/// messages to the client.
pub struct WebSocketProgressCallback {
    tx: mpsc::UnboundedSender<ProgressEvent>,
}

impl WebSocketProgressCallback {
    pub fn new(tx: mpsc::UnboundedSender<ProgressEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl ToolProgressCallback for WebSocketProgressCallback {
    async fn tool_started(&self, call_id: &str, tool_name: &str, args: &Value) {
        let _ = self.tx.send(ProgressEvent::Started {
            call_id: call_id.to_string(),
            tool: tool_name.to_string(),
            args: args.clone(),
        });
    }

    async fn tool_output_delta(&self, call_id: &str, delta: &str) {
        let _ = self.tx.send(ProgressEvent::Delta {
            call_id: call_id.to_string(),
            content: delta.to_string(),
        });
    }

    async fn tool_completed(&self, call_id: &str, _result: &str, success: bool) {
        let _ = self.tx.send(ProgressEvent::Completed {
            call_id: call_id.to_string(),
            success,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn started_event_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cb = WebSocketProgressCallback::new(tx);

        cb.tool_started("c1", "bash", &json!({"command": "ls"}))
            .await;

        let evt = rx.recv().await.unwrap();
        match evt {
            ProgressEvent::Started {
                call_id,
                tool,
                args,
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "ls");
            }
            _ => panic!("expected Started"),
        }
    }

    #[tokio::test]
    async fn delta_event_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cb = WebSocketProgressCallback::new(tx);

        cb.tool_output_delta("c1", "hello world\n").await;

        let evt = rx.recv().await.unwrap();
        match evt {
            ProgressEvent::Delta { call_id, content } => {
                assert_eq!(call_id, "c1");
                assert_eq!(content, "hello world\n");
            }
            _ => panic!("expected Delta"),
        }
    }

    #[tokio::test]
    async fn completed_event_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cb = WebSocketProgressCallback::new(tx);

        cb.tool_completed("c1", "done", true).await;

        let evt = rx.recv().await.unwrap();
        match evt {
            ProgressEvent::Completed { call_id, success } => {
                assert_eq!(call_id, "c1");
                assert!(success);
            }
            _ => panic!("expected Completed"),
        }
    }

    #[tokio::test]
    async fn channel_closed_does_not_panic() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let cb = WebSocketProgressCallback::new(tx);

        // Should silently drop — no panic.
        cb.tool_started("c1", "bash", &json!({})).await;
        cb.tool_output_delta("c1", "output").await;
        cb.tool_completed("c1", "", false).await;
    }

    #[tokio::test]
    async fn full_lifecycle_sequence() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cb = WebSocketProgressCallback::new(tx);

        cb.tool_started("c1", "write_file", &json!({"path": "a.txt"}))
            .await;
        cb.tool_output_delta("c1", "writing...").await;
        cb.tool_completed("c1", "wrote 10 bytes", true).await;

        assert!(matches!(
            rx.recv().await.unwrap(),
            ProgressEvent::Started { .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            ProgressEvent::Delta { .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            ProgressEvent::Completed { .. }
        ));
        assert!(rx.try_recv().is_err()); // No more events.
    }
}
