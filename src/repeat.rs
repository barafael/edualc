//! Something told to the app again and again (a tick, a look whether Claude Code went quiet),
//! as an event on its channel, until stopped: on a thread of its own that sleeps between, or in
//! a page, on its timer. Whatever runs the app's loop, nothing of it is needed.

use std::time::Duration;

use async_channel::Sender;

use crate::app::Event;

/// Sends an event every so often, from `start` until `stop` (or until it is dropped).
#[derive(Debug, Default)]
pub(crate) struct Repeat {
    running: Option<Running>,
}

impl Repeat {
    /// Sends what `event` makes every `period`, from a `period` from now; one running already
    /// is stopped first.
    pub(crate) fn start(&mut self, period: Duration, events: Sender<Event>, event: fn() -> Event) {
        self.running = Some(Running::start(period, events, event));
    }

    pub(crate) fn stop(&mut self) {
        self.running = None;
    }

    pub(crate) fn running(&self) -> bool {
        self.running.is_some()
    }
}

/// A thread that sleeps `period` between events, until its stop is dropped.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct Running {
    /// Dropped to stop the thread: it waits on the other end between events.
    _stop: std::sync::mpsc::Sender<()>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Running {
    fn start(period: Duration, events: Sender<Event>, event: fn() -> Event) -> Self {
        use std::sync::mpsc::RecvTimeoutError;
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                match stopped.recv_timeout(period) {
                    Err(RecvTimeoutError::Timeout) => {
                        if events.try_send(event()).is_err() {
                            tracing::debug!("repeating after the app ended");
                            return;
                        }
                    }
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self { _stop: stop }
    }
}

/// The page's timer, cancelled as it is dropped.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
struct Running {
    _interval: gloo_timers::callback::Interval,
}

#[cfg(target_arch = "wasm32")]
impl Running {
    fn start(period: Duration, events: Sender<Event>, event: fn() -> Event) -> Self {
        let millis = u32::try_from(period.as_millis()).unwrap_or(u32::MAX);
        let interval = gloo_timers::callback::Interval::new(millis, move || {
            if events.try_send(event()).is_err() {
                tracing::debug!("repeating after the app ended");
            }
        });
        Self {
            _interval: interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events come every period once started, and none once stopped.
    #[test]
    fn it_repeats_until_stopped() {
        let (events, received) = async_channel::unbounded();
        let mut repeat = Repeat::default();
        assert!(!repeat.running());
        repeat.start(Duration::from_millis(10), events, || Event::Tick);
        assert!(repeat.running());
        std::thread::sleep(Duration::from_millis(60));
        repeat.stop();
        assert!(!repeat.running());
        let told = std::iter::from_fn(|| received.try_recv().ok()).count();
        assert!(told >= 2, "told {told} times");
        std::thread::sleep(Duration::from_millis(40));
        assert!(received.try_recv().is_err(), "nothing after it stopped");
    }
}
