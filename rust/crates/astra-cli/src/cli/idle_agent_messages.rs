//! Idle agent mailbox draining and prompt-safe rendering.

use super::ReplState;
use crossterm::style::Stylize;

pub(crate) fn drain_root_mailbox_into_idle_queue(state: &mut ReplState) {
    let Some(mailbox) = state.root_mailbox.as_mut() else {
        return;
    };
    while let Some(message) = mailbox.try_recv() {
        state.pending_idle_agent_messages.push(message);
    }
}

fn format_idle_agent_message_payload(payload: &astra_messaging::MessagePayload) -> String {
    use astra_messaging::{AgentSignal, MessagePayload, RequestType};

    match payload {
        MessagePayload::Text { content, summary } => {
            summary.clone().unwrap_or_else(|| content.clone())
        }
        MessagePayload::Progress {
            turn_index,
            tool_calls,
            status,
            detail,
        } => {
            let detail = detail
                .as_ref()
                .map(|text| format!(" — {text}"))
                .unwrap_or_default();
            format!("progress turn {turn_index}, {tool_calls} tool calls: {status}{detail}")
        }
        MessagePayload::Request { request_type, data } => {
            let request = match request_type {
                RequestType::Shutdown => "shutdown".to_string(),
                RequestType::ToolPermission => "tool_permission".to_string(),
                RequestType::ContextShare => "context_share".to_string(),
                RequestType::Custom(name) => format!("custom:{name}"),
            };
            if data.is_null() {
                format!("request {request}")
            } else {
                format!("request {request}: {data}")
            }
        }
        MessagePayload::Response {
            request_id,
            accepted,
            data,
        } => {
            let data = data
                .as_ref()
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            format!(
                "response to {request_id}: {}{data}",
                if *accepted { "accepted" } else { "rejected" }
            )
        }
        MessagePayload::Signal(signal) => match signal {
            AgentSignal::Heartbeat => "heartbeat".to_string(),
            AgentSignal::Idle => "idle".to_string(),
            AgentSignal::Stalled { reason } => format!("stalled: {reason}"),
            AgentSignal::Completed { output } => format!("completed: {output}"),
            AgentSignal::Failed { error } => format!("failed: {error}"),
        },
        MessagePayload::Ack { message_id } => format!("acknowledged {message_id}"),
        MessagePayload::Nack { message_id, reason } => {
            let reason = reason
                .as_ref()
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            format!("rejected {message_id}{reason}")
        }
    }
}

pub(crate) fn flush_idle_agent_messages_between_prompts(state: &mut ReplState) {
    drain_root_mailbox_into_idle_queue(state);
    if state.pending_idle_agent_messages.is_empty() {
        return;
    }

    let pending = std::mem::take(&mut state.pending_idle_agent_messages);
    for message in pending {
        let payload = format_idle_agent_message_payload(&message.payload);
        eprintln!(
            "\n  {} {} {}",
            "mail".cyan(),
            format!("{} -> main", message.from.agent_id).bold(),
            payload
        );
    }
}
