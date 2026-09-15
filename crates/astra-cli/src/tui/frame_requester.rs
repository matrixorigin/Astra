use std::time::{Duration, Instant};

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
    pub(crate) fn new(draw_tx: mpsc::Sender<()>) -> Self {
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
    draw_tx: mpsc::Sender<()>,
    rate_limiter: FrameRateLimiter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawDelivery {
    Sent,
    Pending,
    Closed,
}

impl FrameScheduler {
    fn new(receiver: mpsc::Receiver<Instant>, draw_tx: mpsc::Sender<()>) -> Self {
        Self {
            receiver,
            draw_tx,
            rate_limiter: FrameRateLimiter::default(),
        }
    }

    /// Try to publish one draw wake and advance the limiter only when the
    /// wake was actually accepted by the consumer. A scheduler deadline is
    /// merely a lower bound; it may fire late while the executor is busy, so
    /// using that old deadline as the last-emitted timestamp would let the
    /// next request immediately catch up on obsolete frames.
    fn try_emit_draw_at(&mut self, emitted_at: Instant) -> DrawDelivery {
        match self.draw_tx.try_send(()) {
            Ok(()) => {
                self.rate_limiter.mark_emitted(emitted_at);
                DrawDelivery::Sent
            }
            Err(mpsc::error::TrySendError::Full(())) => DrawDelivery::Pending,
            Err(mpsc::error::TrySendError::Closed(())) => DrawDelivery::Closed,
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
                        // A draw is a wake-up for the latest state, not a
                        // history of every invalidation. Keep the scheduler
                        // independent from the terminal consumer: a stalled
                        // render must never make producers await capacity.
                        // Anchor rate limiting to the actual delivery time;
                        // `target` can be arbitrarily stale after scheduler
                        // starvation. A closed consumer retires the task.
                        if matches!(
                            self.try_emit_draw_at(Instant::now()),
                            DrawDelivery::Closed
                        ) {
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DrawDelivery, FrameRequester, FrameScheduler};
    use crate::tui::frame_rate_limiter::MIN_FRAME_INTERVAL;

    #[tokio::test]
    async fn frame_burst_is_coalesced_without_losing_the_next_draw() {
        let (draw_tx, mut draw_rx) = tokio::sync::mpsc::channel(1);
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

        // Once the pending wake has been consumed, a later mutation still
        // gets its own redraw even though the scheduler output is bounded.
        requester.schedule_frame();
        tokio::time::timeout(std::time::Duration::from_secs(1), draw_rx.recv())
            .await
            .expect("a later request must not be lost")
            .expect("draw channel remains open");
    }

    #[tokio::test]
    async fn stalled_draw_consumer_keeps_one_pending_wake() {
        let (draw_tx, mut draw_rx) = tokio::sync::mpsc::channel(1);
        let requester = FrameRequester::new(draw_tx);
        let frame_interval = super::super::frame_rate_limiter::MIN_FRAME_INTERVAL;
        let producer_period = frame_interval + frame_interval;

        // Leave the consumer stalled while requests arrive over several
        // frame intervals. The first wake represents the latest state when
        // the consumer eventually renders; stale redraws must not accumulate.
        for _ in 0..5 {
            requester.schedule_frame();
            tokio::time::sleep(producer_period).await;
        }

        assert_eq!(draw_rx.try_recv().ok(), Some(()));
        assert!(
            draw_rx.try_recv().is_err(),
            "a stalled consumer must not receive a replay queue of draws"
        );

        // Consuming the pending wake must leave the scheduler usable for a
        // mutation that happens after that draw.
        requester.schedule_frame();
        tokio::time::timeout(std::time::Duration::from_secs(1), draw_rx.recv())
            .await
            .expect("a post-consumption request must produce a draw")
            .expect("draw channel remains open");
    }

    #[test]
    fn late_draw_delivery_anchors_the_next_deadline_to_delivery_time() {
        let (draw_tx, _draw_rx) = tokio::sync::mpsc::channel(1);
        let mut scheduler = FrameScheduler::new(tokio::sync::mpsc::channel(1).1, draw_tx);
        let requested_at = std::time::Instant::now();
        let delivered_at = requested_at + MIN_FRAME_INTERVAL * 10;

        assert_eq!(scheduler.try_emit_draw_at(delivered_at), DrawDelivery::Sent);
        assert_eq!(
            scheduler.rate_limiter.clamp_deadline(delivered_at),
            delivered_at + MIN_FRAME_INTERVAL
        );
    }

    #[test]
    fn failed_draw_delivery_does_not_move_the_rate_limit_clock() {
        let (draw_tx, _draw_rx) = tokio::sync::mpsc::channel(1);
        draw_tx.try_send(()).expect("fill the pending wake slot");
        let mut scheduler = FrameScheduler::new(tokio::sync::mpsc::channel(1).1, draw_tx);
        let attempted_at = std::time::Instant::now() + MIN_FRAME_INTERVAL * 10;

        assert_eq!(
            scheduler.try_emit_draw_at(attempted_at),
            DrawDelivery::Pending
        );
        assert_eq!(
            scheduler.rate_limiter.clamp_deadline(attempted_at),
            attempted_at
        );
    }

    #[test]
    fn closed_draw_consumer_retires_the_scheduler_without_updating_state() {
        let (draw_tx, draw_rx) = tokio::sync::mpsc::channel(1);
        drop(draw_rx);
        let mut scheduler = FrameScheduler::new(tokio::sync::mpsc::channel(1).1, draw_tx);
        let attempted_at = std::time::Instant::now();

        assert_eq!(
            scheduler.try_emit_draw_at(attempted_at),
            DrawDelivery::Closed
        );
        assert_eq!(
            scheduler.rate_limiter.clamp_deadline(attempted_at),
            attempted_at
        );
    }
}
