use crate::debug::Breakpoint;

/// Commands for live parameter tuning of the running harness.
#[derive(Debug, Clone)]
pub enum AdjustCommand {
    /// Update budget verifier limits.
    SetBudgetLimit {
        max_turns: Option<u32>,
        max_tokens: Option<u64>,
        max_duration_millis: Option<u64>,
    },
    /// Add a debug breakpoint.
    AddBreakpoint(Breakpoint),
    /// Clear all breakpoints.
    ClearBreakpoints,
    /// Update cost verifier limit.
    SetCostLimit { max_session_cost_usd: f64 },
    /// Add a tool to the sensitive tools list.
    WatchTool(String),
    /// Remove a tool from the sensitive tools list.
    UnwatchTool(String),
}

/// Response to an adjust command.
#[derive(Debug, Clone)]
pub enum AdjustResponse {
    Ok { message: String },
    Error { message: String },
}

/// Channel pair for sending adjustment commands to the running harness.
pub struct AdjustSender {
    tx: std::sync::mpsc::SyncSender<(AdjustCommand, std::sync::mpsc::SyncSender<AdjustResponse>)>,
}

pub struct AdjustReceiver {
    rx: std::sync::mpsc::Receiver<(AdjustCommand, std::sync::mpsc::SyncSender<AdjustResponse>)>,
}

pub fn adjust_channel(
    bound: usize,
) -> (AdjustSender, AdjustReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel(bound);
    (AdjustSender { tx }, AdjustReceiver { rx })
}

impl AdjustSender {
    pub fn send(&self, cmd: AdjustCommand) -> Option<AdjustResponse> {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(1);
        self.tx.send((cmd, resp_tx)).ok()?;
        resp_rx.recv().ok()
    }
}

impl AdjustReceiver {
    /// Drain and process all pending commands. Returns the number processed.
    pub fn drain(&self, handler: &dyn Fn(AdjustCommand) -> AdjustResponse) -> usize {
        let mut count = 0;
        while let Ok((cmd, resp_tx)) = self.rx.try_recv() {
            let response = handler(cmd);
            let _ = resp_tx.send(response);
            count += 1;
        }
        count
    }
}

impl Clone for AdjustSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_handler(cmd: AdjustCommand) -> AdjustResponse {
        AdjustResponse::Ok {
            message: format!("handled: {cmd:?}"),
        }
    }

    #[test]
    fn send_and_receive() {
        let (sender, receiver) = adjust_channel(4);

        let handle = std::thread::spawn(move || {
            receiver.drain(&noop_handler)
        });

        let resp = sender.send(AdjustCommand::ClearBreakpoints);
        assert!(matches!(resp, Some(AdjustResponse::Ok { .. })));

        let count = handle.join().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn multiple_commands() {
        let (sender, receiver) = adjust_channel(8);

        // drain must run concurrently (send() blocks waiting for response)
        let drain_handle = std::thread::spawn(move || {
            let mut total = 0;
            while total < 3 {
                total += receiver.drain(&noop_handler);
                std::thread::yield_now();
            }
            total
        });

        sender.send(AdjustCommand::SetBudgetLimit {
            max_turns: Some(20),
            max_tokens: None,
            max_duration_millis: None,
        });
        sender.send(AdjustCommand::WatchTool("bash".into()));
        sender.send(AdjustCommand::AddBreakpoint(Breakpoint::AtTurn(5)));

        let count = drain_handle.join().unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn sender_returns_none_after_receiver_drop() {
        let (sender, receiver) = adjust_channel(4);
        drop(receiver);
        assert!(sender.send(AdjustCommand::ClearBreakpoints).is_none());
    }

    #[test]
    fn clone_sender() {
        let (sender, receiver) = adjust_channel(4);
        let sender2 = sender.clone();

        // drain must run concurrently with sends (send() blocks waiting for response)
        let drain_handle = std::thread::spawn(move || {
            let mut total = 0;
            // Keep draining until we've processed 2 commands
            while total < 2 {
                total += receiver.drain(&noop_handler);
                std::thread::yield_now();
            }
            total
        });

        let h1 = std::thread::spawn(move || sender.send(AdjustCommand::ClearBreakpoints));
        let h2 = std::thread::spawn(move || sender2.send(AdjustCommand::WatchTool("x".into())));

        h1.join().unwrap();
        h2.join().unwrap();
        let count = drain_handle.join().unwrap();
        assert_eq!(count, 2);
    }
}
