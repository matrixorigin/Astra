//! Live dashboard: embedded HTTP + WebSocket server for real-time
//! test progress visualization.
//!
//! When `--live-dashboard [PORT]` is passed, the harness starts an
//! axum server that serves a single-page dashboard at `GET /` and
//! pushes `DashboardEvent` JSON over `GET /ws` as cases complete.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::report::{CaseRunReport, SuiteReport};

/// Events pushed over the WebSocket to the dashboard UI.
/// Using untagged serde for the Arc<T> fields since tagged enums
/// with flattened structs have limitations.
#[derive(Debug, Clone)]
pub enum DashboardEvent {
    SuiteStarted {
        total_cases: usize,
        models: Vec<String>,
        started_at: String,
    },
    CaseStarted {
        case_name: String,
        model: String,
        run_index: u32,
    },
    CaseCompleted {
        report: Arc<CaseRunReport>,
    },
    SuiteCompleted {
        report: Arc<SuiteReport>,
    },
}

impl Serialize for DashboardEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::SuiteStarted {
                total_cases,
                models,
                started_at,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "suite_started")?;
                m.serialize_entry("total_cases", total_cases)?;
                m.serialize_entry("models", models)?;
                m.serialize_entry("started_at", started_at)?;
                m.end()
            }
            Self::CaseStarted {
                case_name,
                model,
                run_index,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", "case_started")?;
                m.serialize_entry("case_name", case_name)?;
                m.serialize_entry("model", model)?;
                m.serialize_entry("run_index", run_index)?;
                m.end()
            }
            Self::CaseCompleted { report } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "case_completed")?;
                m.serialize_entry("report", report.as_ref())?;
                m.end()
            }
            Self::SuiteCompleted { report } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "suite_completed")?;
                m.serialize_entry("report", report.as_ref())?;
                m.end()
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<DashboardEvent>,
}

pub struct DashboardServer {
    tx: broadcast::Sender<DashboardEvent>,
}

impl Default for DashboardServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardServer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self { tx }
    }

    pub fn sender(&self) -> broadcast::Sender<DashboardEvent> {
        self.tx.clone()
    }

    pub async fn start(self, port: u16) -> anyhow::Result<()> {
        let state = AppState { tx: self.tx };
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        eprintln!("[astra-test] dashboard: http://localhost:{port}");
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../dashboard.html"))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut rx = state.tx.subscribe();
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let msg = serde_json::json!({"type": "lagged", "missed": n});
                        let _ = socket.send(Message::Text(msg.to_string().into())).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }
}
