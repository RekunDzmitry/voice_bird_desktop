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
//! ## Why not `Copy`?
//!
//! `DownloadFailed` carries a `String` so the rendered error line is
//! actionable ("HTTP 404" beats "network error"). That forfeits `Copy`
//! for the whole enum. `Eq` survives — every payload stays integral;
//! keep it that way. The moment a payload gains an `f64` (a ratio, a
//! duration) the derive breaks and every `assert_eq!` in this file's
//! tests stops compiling, which is why progress is `bytes`/`total` and
//! never a pre-computed ratio.

use std::sync::mpsc;

use crate::picker::{ModelEntry, PickerMove};

/// Direction focus travelled between blocks. The reducer saturates at
/// both ends; the key layer never has to reason about wraparound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FocusMove {
    Prev,
    Next,
}

/// Everything that can happen in the app, as plain data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event")]
pub enum AppEvent {
    /// `+`: push a `Picking` block and focus it. Allowed at any time.
    AddBlock,
    /// `←` / `→`: move focus between blocks.
    FocusMoved { direction: FocusMove },
    /// `↑` / `↓` while the focused block is `Picking`: move the
    /// highlight inside that block's catalog list.
    PickerMoved { direction: PickerMove },
    /// Enter on a focused `Picking` block. Today this transitions
    /// straight to `Recording`; step 8 routes it through the resolver
    /// (`begin`).
    ModelSelected(&'static ModelEntry),
    /// Esc on the focused block: remove it.
    BlockClosed,
    /// Model is on disk and ready.
    RecordingStarted(&'static ModelEntry),
    /// The focused block now waits on this model.
    DownloadRequested(&'static ModelEntry),
    /// Progress on a download.
    DownloadProgress {
        model: &'static str,
        bytes: u64,
        total: Option<u64>,
    },
    /// Bytes verified; the format handler is unpacking.
    DownloadInstalling { model: &'static str },
    /// Fans out to EVERY block waiting on `model`.
    DownloadSucceeded { model: &'static str },
    /// `error` carries an actionable message.
    DownloadFailed {
        model: &'static str,
        error: String,
    },
    /// Last waiter for `model` closed. Removes the repo row and the
    /// UiState.downloads entry; the in-flight thread observes the
    /// cancel flag separately and publishes nothing of its own.
    DownloadCancelled { model: &'static str },
    /// `q` / Ctrl-C: quit. Always honoured, including mid-download.
    Quit,
}

/// Cloneable producer handle. Producers only need this — `publish` is the
/// whole API they see, and cloning is the only way to get one.
#[derive(Debug, Clone)]
pub struct EventSender(pub mpsc::Sender<AppEvent>);

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

    #[test]
    fn publish_after_drop_is_a_silent_no_op() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let bus = EventBus::new();
        let tx = bus.sender();
        drop(bus);

        assert!(
            tx.0.send(AppEvent::Quit).is_err(),
            "underlying channel should report disconnected after the bus is dropped",
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            tx.publish(AppEvent::AddBlock);
            tx.publish(AppEvent::Quit);
        }));
        assert!(result.is_ok(), "publish after drop must not panic");
    }

    #[test]
    fn dropped_sender_does_not_deliver_to_a_fresh_bus() {
        let stale = EventBus::new();
        let stale_tx = stale.sender();
        drop(stale);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stale_tx.publish(AppEvent::AddBlock);
        }));
        assert!(result.is_ok());
        assert!(stale_tx.0.send(AppEvent::AddBlock).is_err());

        let mut fresh = EventBus::new();
        assert_eq!(fresh.drain().count(), 0);
    }
}