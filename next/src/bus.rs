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
    /// highlight inside that block's catalog list. `from_model` and
    /// `to_model` are stamped by the resolver; the input layer has
    /// no catalog context. Tests (and any future event source that
    /// doesn't know the focused block) pass `None, None`.
    PickerMoved {
        direction: PickerMove,
        from_model: Option<&'static str>,
        to_model: Option<&'static str>,
    },
    /// Enter on a focused `Picking` block. Today this transitions
    /// straight to `Recording`; step 8 routes it through the resolver
    /// (`begin`).
    ModelSelected(&'static ModelEntry),
    /// Esc on the focused block: remove it.
    BlockClosed,
    /// Model is on disk and ready.
    RecordingStarted(&'static ModelEntry),
    /// Resolver detected the model is already on disk before any
    /// download was attempted. Published alongside `RecordingStarted`
    /// on the cache-hit path so the event log records *why* the
    /// block transitioned straight to `Recording` without a
    /// `DownloadRequested`. Reducer treats this as an alias for
    /// `RecordingStarted`.
    ModelAlreadyCached(&'static ModelEntry),

    /// The focused block now waits on this model.
    DownloadRequested(&'static ModelEntry),
    /// Progress on a download. `bytes_per_sec` is a per-tick average
    /// over the throttle window — used by the renderer to label the
    /// gauge when `total` is `None`. `attempt` disambiguates events
    /// from concurrent attempts of the same model: the store
    /// discards any event whose attempt does not match the row's
    /// current attempt.
    DownloadProgress {
        attempt: u32,
        model: &'static str,
        bytes: u64,
        total: Option<u64>,
        bytes_per_sec: u64,
    },
    /// Bytes verified; the format handler is unpacking.
    DownloadInstalling { attempt: u32, model: &'static str },
    /// Fans out to EVERY block waiting on `model`.
    DownloadSucceeded { attempt: u32, model: &'static str },
    /// `error` carries an actionable message.
    DownloadFailed {
        attempt: u32,
        model: &'static str,
        error: String,
    },
    /// Last waiter for `model` closed. Removes the repo row and the
    /// UiState.downloads entry; the in-flight thread observes the
    /// cancel flag separately and publishes nothing of its own.
    DownloadCancelled { attempt: u32, model: &'static str },
    /// `Tab`: open (or close, if already open) the session menu.
    MenuOpened,
    /// `Esc` (or `Tab` again) while the menu is open.
    MenuClosed,
    /// `↑` / `↓` while the menu is open. Reuses the picker's move
    /// direction so clamp semantics live in one place.
    MenuMoved { direction: PickerMove },
    /// `Enter` on a menu row: reveal the selected session on screen,
    /// FIFO-evicting the oldest-focused visible one. Reducer closes
    /// the menu itself.
    SessionShown { id: u8 },
    /// `q` / Ctrl-C: quit. Always honoured, including mid-download.
    Quit,
}

/// Cloneable producer handle. Producers only need this — `publish` is the
/// whole API they see, and cloning is the only way to get one.
#[derive(Debug, Clone)]
pub struct EventSender(mpsc::Sender<AppEvent>);

impl EventSender {
    /// Wrap an `mpsc::Sender`. Sole constructor — the field is
    /// private so producers have to come through [`EventBus`],
    /// which is what guarantees the bus and its senders are
    /// constructed together. Tests that used to inline the tuple
    /// (`EventSender(mpsc::channel().0)`) now go through
    /// `EventBus::new().sender()` to keep the same coupling.
    pub fn from_mpsc(sender: mpsc::Sender<AppEvent>) -> Self {
        Self(sender)
    }

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

    pub fn sender(&self) -> EventSender {
        EventSender::from_mpsc(self.sender.clone())
    }

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

    #[test]
    fn menu_events_serialize_under_the_event_tag() {
        // The on-disk event log replays via the `event` discriminator
        // that `#[serde(tag = "event")]` writes onto every variant.
        // New variants must keep that contract — see bus.rs header.
        let events = vec![
            AppEvent::MenuOpened,
            AppEvent::MenuClosed,
            AppEvent::MenuMoved {
                direction: PickerMove::Down,
            },
            AppEvent::SessionShown { id: 4 },
        ];
        let json = serde_json::to_string(&events).expect("serialize");
        assert!(json.contains("\"event\":\"MenuOpened\""), "{json}");
        assert!(json.contains("\"event\":\"MenuClosed\""), "{json}");
        assert!(json.contains("\"event\":\"MenuMoved\""), "{json}");
        assert!(json.contains("\"event\":\"SessionShown\""), "{json}");
        assert!(json.contains("\"id\":4"), "{json}");
    }
}