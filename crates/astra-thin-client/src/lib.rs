//! Stateless HTTP + SSE client for the thin client protocol (`docs/design/multi-agent-cloud-runtime.md` §5.5).
//!
//! ## Layers
//! - [`paths`] — URL paths shared by CLI, Web, and IDE clients (aligned with `docs/design/multi-agent-cloud-runtime.md` §5.5 and `router_builder.rs`).
//! - [`protocol`] — JSON request bodies and [`protocol::StreamEvent`] classification.
//! - [`edge`] — lightweight edge executor metadata (`edge_executor_id`, capability presets, §5.5.2).
//! - [`sse`] — incremental `data: …\\n\\n` parser matching the server SSE framing (`data: {json}\\n\\n`).
//! - [`ThinClient`](client::ThinClient) — `reqwest`-based transport.
//!
//! The crate deliberately avoids `runtime` / `services` so any front-end can depend on it without pulling the cognitive engine. It depends on `astra-runtime-env` only for the shared runtime capability vocabulary.

pub mod client;
pub mod device_proof;
pub mod edge;
pub mod error;
pub mod paths;
pub mod protocol;
pub mod sse;

pub use astra_server_types::{
    CompletionChoice, CompletionMessage, CompletionOperation, CompletionRequest,
    CompletionResponse, CompletionUsage, SESSION_RUN_TREE_SCHEMA_VERSION, SessionRunAction,
    SessionRunCapabilityServerRefs, SessionRunLifecycleStatus, SessionRunNode,
    SessionRunPermissionFacts, SessionRunRuntimeFacts, SessionRunTreeSnapshot,
};
pub use client::ThinClient;
pub use device_proof::{
    ASTRA_DEVICE_CHALLENGE_ID_HEADER, ASTRA_DEVICE_FINGERPRINT_HEADER, ASTRA_DEVICE_ID_HEADER,
    ASTRA_DEVICE_PROOF_HEADER, DeviceProofPurpose, canonical_device_proof_message,
    device_challenge_proof,
};
pub use edge::{
    ASTRA_EDGE_ID_HEADER, advertise_executor, builtin_capability_preset,
    edge_register_with_capabilities, edge_runtime_environment_capabilities,
};
pub use error::ThinClientError;
pub use protocol::{
    ApprovalDecision, ApprovalKind, ApprovalRespondRequest, ChatStreamRequest,
    EdgeHeartbeatReplayPolicy, EdgeHeartbeatRequest, EdgeHeartbeatResponse, EdgeRegisterRequest,
    LegacyEdgePendingRequest, ModelSelection, RunUserIntentRequest, RunUserIntentResponse,
    SessionCreateRequest, SessionTranscriptItem, SessionTranscriptPage, SessionTranscriptPageRef,
    SessionTranscriptReadScope, SessionTranscriptToolCall, SessionTranscriptToolResult,
    SessionUpdateRequest, StreamEvent, TaskLeaseMutationRequest, ToolResultRequest,
    ToolResultRequestParts, UserPromptRespondRequest, classify_stream_event,
};
/// SSE / buffered HTTP response from [`ThinClient::post_chat_turn`] (transport type for consumers like CLI stream rendering).
pub use reqwest::Response as HttpResponse;
