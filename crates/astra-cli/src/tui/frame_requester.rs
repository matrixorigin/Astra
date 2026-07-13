use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::sync::mpsc;

use super::frame_rate_limiter::FrameRateLimiter;

#[derive(Clone, Debug)]
pub(crate) struct FrameRequester {
    // A pending frame is a wake-up, not an event log. One queued wake is
    // enough to render every state mutation that happened before the draw;
    // keeping more only turns a token/agent burst into an unbounded memory
    // queue and stale redraw work.
    frame_schedule_tx: mpsc::Sender<Instant>,
}

impl FrameRequester {
    pub(crate) fn new(draw_tx: broadcast::Sender<()>) -> Self {
        let (tx, rx) = mpsc::channel(1);
        let scheduler = FrameScheduler::new(rx, draw_tx);
        tokio::spawn(scheduler.run());
        Self {
            frame_schedule_tx: tx,
        }
    }

    pub(crate) fn schedule_frame(&self) {
        // If a wake is already pending, the next frame sees the newest state
        // because the reducer owns that state before requesting redraw. Do
        // not await here: render scheduling must never delay keyboard or
        // stream handling.
        let _ = self.frame_schedule_tx.try_send(Instant::now());
    }
}

#[cfg(test)]
impl FrameRequester {
    pub(crate) fn test_dummy() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        FrameRequester {
            frame_schedule_tx: tx,
        }
    }
}

struct FrameScheduler {
    receiver: mpsc::Receiver<Instant>,
    draw_tx: broadcast::Sender<()>,
    rate_limiter: FrameRateLimiter,
}

impl FrameScheduler {
    fn new(receiver: mpsc::Receiver<Instant>, draw_tx: broadcast::Sender<()>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::FrameRequester;

    #[tokio::test]
    async fn frame_burst_is_coalesced_without_losing_the_next_draw() {
        let (draw_tx, mut draw_rx) = tokio::sync::broadcast::channel(16);
        let requester = FrameRequester::new(draw_tx);

        for _ in 0..50_000 {
            requester.schedule_frame();
        }

        tokio::time::timeout(std::time::Duration::from_secs(1), draw_rx.recv())
            .await
            .expect("a coalesced wake must still produce a draw")
            .expect("draw channel remains open");

        // A burst must not be replayed as a long queue of obsolete frames.
        // The scheduler may emit one follow-up when it races the producer,
        // but should become quiet promptly after the burst stops.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut follow_ups = 0usize;
        while draw_rx.try_recv().is_ok() {
            follow_ups += 1;
        }
        assert!(
            follow_ups <= 2,
            "coalesced burst emitted {follow_ups} obsolete follow-up frames"
        );
    }
}
