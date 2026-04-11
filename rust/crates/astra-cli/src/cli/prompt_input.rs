//! Prompt input waiting and normalization helpers for the REPL loop.

use rustyline::error::ReadlineError;

use super::{
    ReplState,
    idle_agent_messages::flush_idle_agent_messages_between_prompts,
    readline_actor::{ReadlineActor, ReadlineResponse},
};

enum PromptWaitOutcome {
    Readline(Result<String, ReadlineError>, Option<String>),
    IdleAgentMessage(Option<std::sync::Arc<astra_runtime::messaging::AgentMessage>>),
}

pub(crate) async fn wait_for_prompt_input(
    state: &mut ReplState,
    readline: &mut ReadlineActor,
    prompt_str: String,
) -> (Result<String, ReadlineError>, Option<String>) {
    readline.request_readline(prompt_str);

    let result = loop {
        let outcome = if let Some(mailbox) = state.root_mailbox.as_ref() {
            tokio::select! {
                result = readline.recv() => match result {
                    Some(ReadlineResponse::Line { result, pending_execute }) => {
                        PromptWaitOutcome::Readline(result, pending_execute)
                    }
                    None => PromptWaitOutcome::Readline(Err(ReadlineError::Eof), None),
                },
                message = mailbox.recv() => PromptWaitOutcome::IdleAgentMessage(message),
            }
        } else {
            match readline.recv().await {
                Some(ReadlineResponse::Line {
                    result,
                    pending_execute,
                }) => PromptWaitOutcome::Readline(result, pending_execute),
                None => PromptWaitOutcome::Readline(Err(ReadlineError::Eof), None),
            }
        };

        match outcome {
            PromptWaitOutcome::Readline(result, pending_execute) => {
                break (result, pending_execute);
            }
            PromptWaitOutcome::IdleAgentMessage(Some(message)) => {
                state.pending_idle_agent_messages.push(message);
            }
            PromptWaitOutcome::IdleAgentMessage(None) => {
                state.root_mailbox = None;
            }
        }
    };

    flush_idle_agent_messages_between_prompts(state);
    result
}

pub(crate) fn normalize_repl_input(line: &str) -> String {
    line.lines()
        .map(|part| part.strip_suffix('\\').unwrap_or(part))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::normalize_repl_input;

    #[test]
    fn normalize_repl_input_joins_multiline_continuations() {
        assert_eq!(normalize_repl_input("hello\\\nworld"), "hello\nworld");
    }

    #[test]
    fn normalize_repl_input_trims_outer_whitespace() {
        assert_eq!(normalize_repl_input("  hi there  "), "hi there");
    }
}
