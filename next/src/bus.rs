//! Application event bus.
//!
//! Transport for everything that can happen in the app. Producers (today: the
//! input path; tomorrow: background adapters — audio devices, engines, timers)
//! call [`EventSender::publish`]. The loop drains queued events at the bottom
//! of each tick and feeds them to a reducer that folds them into `UiState`.
//!
//! ## Why a typed enum + std `mpsc`
//!
//! One producer, many consumers is the only pattern the loop needs today;
//! many producers, one consumer is the only one that needs a queue. Both
//! are covered by `std::sync::mpsc` — `EventSender` is `Clone`, so producers
//! only ever touch [`EventSender::publish`], and `drain` yields in publish
//! order on the loop thread.
//!
//! ## Upgrade paths without reshaping `publish`
//!
//! - **Many producers** — already here: clone the [`EventSender`].
//! - **Many consumers** — either dispatch each drained event to several
//!   reducers in `run`, or grow `subscribe() -> Subscription` with internal
//!   fan-out. Neither changes the public producer surface.
//! - **Topics** — match on [`AppEvent`] variants (or a nested enum) at
//!   dispatch time; the transport never has to know.
//!
//! Deliberately not built now: a generic `EventBus<T>`, topic registration,
//! per-subscriber queues, crossbeam, tokio.

use std::sync::mpsc;

/// Everything that can happen in the app, as plain data.
///
/// Named `AppEvent` to avoid colliding with `crossterm::event::Event`
/// (imported in `main.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEvent {
    /// The user pressed `'+'`. Reducer grows the block count by one.
    AddBlock,
    /// The user asked to quit (`q`, `Esc`, or `Ctrl-C`).
    Quit,
}

/// Cloneable producer handle. Producers only need this — `publish` is the
/// whole API they see, and cloning is the only way to get one.
#[derive(Debug, Clone)]
pub struct EventSender(mpsc::Sender<AppEvent>);

impl EventSender {
    /// Best-effort: send only fails when the bus is gone, i.e. the loop is
    /// shutting down — dropping the event is correct then.
    pub fn publish(&self, event: AppEvent) {
        let _ = self.0.send(event);
    }
}

/// Single-consumer pub/sub over `std::sync::mpsc`.
pub struct EventBus {
    sender: mpsc::Sender<AppEvent>,
    receiver: mpsc::Receiver<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { sender, receiver }
    }

    /// Cloneable handle that producers use to publish.
    pub fn sender(&self) -> EventSender {
        EventSender(self.sender.clone())
    }

    /// Non-blocking: yields every queued event in publish order, then stops.
    /// Called once per tick from the event loop.
    pub fn drain(&mut self) -> impl Iterator<Item = AppEvent> + '_ {
        self.receiver.try_iter()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_yields_events_in_publish_order() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        tx.publish(AppEvent::AddBlock);
        tx.publish(AppEvent::AddBlock);
        tx.publish(AppEvent::Quit);
        let got: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(
            got,
            vec![AppEvent::AddBlock, AppEvent::AddBlock, AppEvent::Quit]
        );
    }

    #[test]
    fn second_drain_is_empty() {
        let mut bus = EventBus::new();
        bus.sender().publish(AppEvent::Quit);
        let _ = bus.drain().count();
        assert_eq!(bus.drain().count(), 0);
    }

    #[test]
    fn two_cloned_senders_interleave_in_publish_order() {
        let mut bus = EventBus::new();
        let a = bus.sender();
        let b = bus.sender();
        a.publish(AppEvent::AddBlock);
        b.publish(AppEvent::AddBlock);
        a.publish(AppEvent::AddBlock);
        b.publish(AppEvent::Quit);
        let got: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(
            got,
            vec![
                AppEvent::AddBlock,
                AppEvent::AddBlock,
                AppEvent::AddBlock,
                AppEvent::Quit,
            ]
        );
    }

    /// Once the bus is dropped, every `sender` clone is disconnected:
    /// subsequent `publish` calls must neither panic nor ever reach a
    /// consumer. The previous version only asserted "does not panic" by
    /// virtue of running — this version makes both halves explicit so a
    /// future refactor (e.g. one that returns the `Result` from `publish`,
    /// or that spawns a producer thread) cannot silently weaken the
    /// contract.
    #[test]
    fn publish_after_drop_is_a_silent_no_op() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let bus = EventBus::new();
        let tx = bus.sender();
        drop(bus);

        // Reach the underlying `mpsc::Sender` to confirm the channel is
        // disconnected: `send` returns `Err(SendError(_))` once the
        // receiver is gone. `publish` wraps this and must agree by
        // silently dropping the event instead of panicking.
        assert!(
            tx.0.send(AppEvent::Quit).is_err(),
            "underlying channel should report disconnected after the bus is dropped",
        );

        // No panic on publish, even though the channel is closed.
        let result = catch_unwind(AssertUnwindSafe(|| {
            tx.publish(AppEvent::AddBlock);
            tx.publish(AppEvent::Quit);
        }));
        assert!(result.is_ok(), "publish after drop must not panic");
    }

    /// A fresh bus has nothing queued: draining yields zero events even
    /// when previous buses were dropped mid-flight. Guards the "consumer
    /// received nothing" half of the drop contract at the bus level.
    #[test]
    fn dropped_sender_does_not_deliver_to_a_fresh_bus() {
        // Drop a sender's bus before publishing.
        let stale = EventBus::new();
        let stale_tx = stale.sender();
        drop(stale);

        // `stale_tx` is now disconnected; publishes must be silent no-ops.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stale_tx.publish(AppEvent::AddBlock);
        }));
        assert!(result.is_ok());
        assert!(stale_tx.0.send(AppEvent::AddBlock).is_err());

        // A brand-new bus has its own channel; nothing from the stale one
        // can leak into it.
        let mut fresh = EventBus::new();
        assert_eq!(fresh.drain().count(), 0);
    }
}
