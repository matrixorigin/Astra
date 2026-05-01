use std::time::Duration;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::sync::mpsc;

use super::frame_rate_limiter::FrameRateLimiter;

#[derive(Clone, Debug)]
pub(crate) struct FrameRequester {
    frame_schedule_tx: mpsc::UnboundedSender<Instant>,
}

impl FrameRequester {
    pub(crate) fn new(draw_tx: broadcast::Sender<()>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let scheduler = FrameScheduler::new(rx, draw_tx);
        tokio::spawn(scheduler.run());
        Self {
            frame_schedule_tx: tx,
        }
    }

    pub(crate) fn schedule_frame(&self) {
        let _ = self.frame_schedule_tx.send(Instant::now());
    }

    pub(crate) fn schedule_frame_in(&self, dur: Duration) {
        let _ = self.frame_schedule_tx.send(Instant::now() + dur);
    }
}

#[cfg(test)]
impl FrameRequester {
    pub(crate) fn test_dummy() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        FrameRequester {
            frame_schedule_tx: tx,
        }
    }
}

struct FrameScheduler {
    receiver: mpsc::UnboundedReceiver<Instant>,
    draw_tx: broadcast::Sender<()>,
    rate_limiter: FrameRateLimiter,
}

impl FrameScheduler {
    fn new(receiver: mpsc::UnboundedReceiver<Instant>, draw_tx: broadcast::Sender<()>) -> Self {
        Self {
            receiver,
            draw_tx,
            rate_limiter: FrameRateLimiter::default(),
        }
    }

    async fn run(mut self) {
        const ONE_YEAR: Duration = Duration::from_secs(60 * 60 * 24 * 365);
        let mut next_deadline: Option<Instant> = None;
        loop {
            let target = next_deadline.unwrap_or_else(|| Instant::now() + ONE_YEAR);
            let deadline = tokio::time::sleep_until(target.into());
            tokio::pin!(deadline);

            tokio::select! {
                draw_at = self.receiver.recv() => {
                    let Some(draw_at) = draw_at else {
                        break
                    };
                    let draw_at = self.rate_limiter.clamp_deadline(draw_at);
                    next_deadline = Some(next_deadline.map_or(draw_at, |cur| cur.min(draw_at)));
                    continue;
                }
                _ = &mut deadline => {
                    if next_deadline.is_some() {
                        next_deadline = None;
                        self.rate_limiter.mark_emitted(target);
                        let _ = self.draw_tx.send(());
                    }
                }
            }
        }
    }
}
