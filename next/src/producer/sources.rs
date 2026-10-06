//! Capturable devices, running apps, and the producer that refreshes them.
//!
//! Native enumeration runs on Tokio's blocking pool, serialized per session.
//! A panic disables the catalog for the session; every request still receives
//! a language-first fallback. Only macOS currently supplies source snapshots.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bus::{AppEvent, EventSender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DeviceKind {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioDevice {
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AppTarget {
    pub id: String,
    pub name: String,
    pub pid: u32,
}

/// Point-in-time enumeration of audio devices and running apps, refreshed per new block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioSourceSnapshot {
    pub devices: Vec<AudioDevice>,
    pub apps: Vec<AppTarget>,
}

/// The source funnel's persisted, revision-gated position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FunnelStep {
    Device,
    App,
    Language,
    Committed,
}

impl FunnelStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "Device",
            Self::App => "App",
            Self::Language => "Language",
            Self::Committed => "Committed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Device" => Some(Self::Device),
            "App" => Some(Self::App),
            "Language" => Some(Self::Language),
            "Committed" => Some(Self::Committed),
            _ => None,
        }
    }
}

/// UI mirror of the current source selections and persisted revision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceSelection {
    pub snapshot: AudioSourceSnapshot,
    pub device: Option<AudioDevice>,
    pub app: Option<AppTarget>,
    pub rev: u32,
}

impl SourceSelection {
    /// Chosen device and app labels, in block-title order.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.device
            .iter()
            .map(|device| device.name.as_str())
            .chain(self.app.iter().map(|app| app.name.as_str()))
    }
}

pub trait SourceCatalog: Send + Sync {
    /// `None` means source enumeration is unavailable on this host.
    fn snapshot(&self) -> Option<AudioSourceSnapshot>;
}

thread_local! {
    static SOURCE_QUERY_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current panic is inside a recoverable native source query.
/// The binary's panic hook must leave the live terminal untouched here.
pub fn source_query_panicking() -> bool {
    std::thread::panicking() && SOURCE_QUERY_ACTIVE.get()
}

struct SourceQueryActive(bool);

impl SourceQueryActive {
    fn enter() -> Self {
        Self(SOURCE_QUERY_ACTIVE.replace(true))
    }
}

impl Drop for SourceQueryActive {
    fn drop(&mut self) {
        SOURCE_QUERY_ACTIVE.set(self.0);
    }
}

/// Starts source queries without blocking the consumer or waiting on Drop.
/// Requests retain their own handles, so queued replies survive manager Drop.
pub struct SourceManager {
    catalog: Arc<dyn SourceCatalog>,
    requests: Arc<Mutex<()>>,
    failed: Arc<AtomicBool>,
}

impl SourceManager {
    pub fn new(catalog: Arc<dyn SourceCatalog>) -> Self {
        Self {
            catalog,
            requests: Arc::new(Mutex::new(())),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Publish one source-picker or language-picker answer for this request.
    pub fn request(&self, tx: &EventSender) {
        let catalog = self.catalog.clone();
        let requests = self.requests.clone();
        let failed = self.failed.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _request = requests.lock().await;
            let snapshot = if failed.load(Ordering::Acquire) {
                None
            } else {
                match tokio::task::spawn_blocking(move || {
                    // The guard is still alive when the panic hook runs, and
                    // restores the blocking worker's thread-local on unwind.
                    let _query = SourceQueryActive::enter();
                    catalog.snapshot()
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        if error.is_panic() {
                            // Native state may be inconsistent after unwinding.
                            failed.store(true, Ordering::Release);
                        }
                        None
                    }
                }
            };
            tx.publish(match snapshot {
                Some(snapshot) if !snapshot.devices.is_empty() => {
                    AppEvent::AddSourceBlock { snapshot }
                }
                _ => AppEvent::AddBlock,
            });
        });
    }
}

/// Language-first fallback, also useful for deterministic tests on macOS.
pub struct NoSources;

impl SourceCatalog for NoSources {
    fn snapshot(&self) -> Option<AudioSourceSnapshot> {
        None
    }
}

pub fn system_sources() -> Arc<dyn SourceCatalog> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacSources)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(NoSources)
    }
}

#[cfg(target_os = "macos")]
pub struct MacSources;

#[cfg(target_os = "macos")]
impl SourceCatalog for MacSources {
    fn snapshot(&self) -> Option<AudioSourceSnapshot> {
        macos::snapshot()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::{BTreeSet, HashSet};

    use cpal::traits::{DeviceTrait, HostTrait};
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

    use super::{AppTarget, AudioDevice, AudioSourceSnapshot, DeviceKind};

    pub(super) fn snapshot() -> Option<AudioSourceSnapshot> {
        let host = cpal::default_host();
        let mut inputs = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        let mut available = false;

        if let Ok(devices) = host.input_devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    inputs.insert(name);
                }
            }
        }
        if let Ok(devices) = host.output_devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    outputs.insert(name);
                }
            }
        }
        // cpal can omit output-only devices with no supported output configs.
        // Sweep all devices as well, excluding names already known as inputs.
        // Explicit output enumeration still preserves duplex devices in both axes.
        if let Ok(devices) = host.devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    if !inputs.contains(&name) {
                        outputs.insert(name);
                    }
                }
            }
        }
        if !available {
            return None;
        }

        let devices = inputs
            .into_iter()
            .map(|name| AudioDevice {
                name,
                kind: DeviceKind::Input,
            })
            .chain(outputs.into_iter().map(|name| AudioDevice {
                name,
                kind: DeviceKind::Output,
            }))
            .collect();
        Some(AudioSourceSnapshot {
            devices,
            apps: running_apps(),
        })
    }

    /// NSWorkspace includes minimized and tray apps without Screen Recording
    /// permission, unlike SCShareableContent's shareable-window list.
    fn running_apps() -> Vec<AppTarget> {
        // Tokio's blocking workers have no Cocoa run loop to drain these objects.
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let running = workspace.runningApplications();
            let mut apps = Vec::new();
            let mut seen = HashSet::new();

            for index in 0..running.count() {
                let app = running.objectAtIndex(index);
                if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                    continue;
                }
                let Some(name) = app.localizedName() else {
                    continue;
                };
                let name = name.to_string();
                if name.is_empty() {
                    continue;
                }
                let id = app
                    .bundleIdentifier()
                    .map(|bundle| bundle.to_string())
                    .filter(|bundle| !bundle.is_empty())
                    .unwrap_or_else(|| name.clone());
                if !seen.insert(id.clone()) {
                    continue;
                }
                apps.push(AppTarget {
                    id,
                    name,
                    pid: app.processIdentifier().max(0) as u32,
                });
            }

            apps.sort_by_cached_key(|app| app.name.to_lowercase());
            apps
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Mutex as BlockingMutex;
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    use super::*;
    use crate::bus::EventBus;
    use crate::testing::{sample_source_snapshot, FixtureSources};

    // Deadlock watchdog only; channels establish request and release ordering.
    const WAIT: Duration = Duration::from_secs(5);

    async fn reply(bus: &mut EventBus) -> AppEvent {
        timeout(WAIT, bus.recv()).await.unwrap().unwrap()
    }

    async fn request_block(snapshot: Option<AudioSourceSnapshot>) -> AppEvent {
        let mut bus = EventBus::new();
        let manager = SourceManager::new(Arc::new(FixtureSources(snapshot)));
        manager.request(&bus.sender());
        reply(&mut bus).await
    }

    #[tokio::test]
    async fn request_block_starts_source_funnel_when_devices_exist() {
        let snapshot = sample_source_snapshot();
        assert_eq!(
            request_block(Some(snapshot.clone())).await,
            AppEvent::AddSourceBlock { snapshot }
        );
    }

    #[tokio::test]
    async fn request_block_falls_back_when_sources_are_unavailable() {
        assert_eq!(request_block(None).await, AppEvent::AddBlock);
    }

    #[tokio::test]
    async fn request_block_falls_back_when_only_apps_exist() {
        let mut snapshot = sample_source_snapshot();
        snapshot.devices.clear();
        assert_eq!(request_block(Some(snapshot)).await, AppEvent::AddBlock);
    }

    #[tokio::test]
    async fn request_block_falls_back_when_snapshot_is_empty() {
        assert_eq!(
            request_block(Some(AudioSourceSnapshot {
                devices: Vec::new(),
                apps: Vec::new(),
            }))
            .await,
            AppEvent::AddBlock
        );
    }

    #[tokio::test]
    async fn request_block_keeps_source_funnel_when_no_apps_are_running() {
        let mut snapshot = sample_source_snapshot();
        snapshot.apps.clear();
        assert_eq!(
            request_block(Some(snapshot.clone())).await,
            AppEvent::AddSourceBlock { snapshot }
        );
    }

    #[tokio::test]
    async fn panicking_catalog_answers_current_queued_and_future_requests() {
        struct QueryUnwind(Arc<AtomicBool>);

        impl Drop for QueryUnwind {
            fn drop(&mut self) {
                self.0.store(source_query_panicking(), Ordering::SeqCst);
            }
        }

        struct PanickingCatalog {
            entered: mpsc::UnboundedSender<()>,
            release: BlockingMutex<Option<oneshot::Receiver<()>>>,
            calls: AtomicUsize,
            recoverable_unwind: Arc<AtomicBool>,
        }

        impl SourceCatalog for PanickingCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _unwind = QueryUnwind(self.recoverable_unwind.clone());
                self.entered.send(()).unwrap();
                self.release
                    .blocking_lock()
                    .take()
                    .unwrap()
                    .blocking_recv()
                    .unwrap();
                panic!("native enumeration failed");
            }
        }

        let mut bus = EventBus::new();
        let tx = bus.sender();
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (release, release_rx) = oneshot::channel();
        let recoverable_unwind = Arc::new(AtomicBool::new(false));
        let catalog = Arc::new(PanickingCatalog {
            entered: entered_tx,
            release: BlockingMutex::new(Some(release_rx)),
            calls: AtomicUsize::new(0),
            recoverable_unwind: recoverable_unwind.clone(),
        });
        let manager = SourceManager::new(catalog.clone());
        manager.request(&tx);
        timeout(WAIT, entered.recv()).await.unwrap().unwrap();
        manager.request(&tx);
        release.send(()).unwrap();
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        manager.request(&tx);
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
        assert!(recoverable_unwind.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn requests_are_serialized_and_each_enumerates_a_fresh_snapshot() {
        struct SerializedCatalog {
            entered: mpsc::UnboundedSender<(usize, usize)>,
            release: BlockingMutex<mpsc::UnboundedReceiver<()>>,
            calls: AtomicUsize,
            active: AtomicUsize,
        }

        impl SourceCatalog for SerializedCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.entered.send((call, active)).unwrap();
                self.release.blocking_lock().blocking_recv().unwrap();
                self.active.fetch_sub(1, Ordering::SeqCst);
                (call == 0).then(sample_source_snapshot)
            }
        }

        let mut bus = EventBus::new();
        let tx = bus.sender();
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (release, release_rx) = mpsc::unbounded_channel();
        let catalog = Arc::new(SerializedCatalog {
            entered: entered_tx,
            release: BlockingMutex::new(release_rx),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        });
        let manager = SourceManager::new(catalog.clone());
        manager.request(&tx);
        assert_eq!(
            timeout(WAIT, entered.recv()).await.unwrap().unwrap(),
            (0, 1)
        );
        manager.request(&tx);
        tokio::task::yield_now().await;
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
        assert!(entered.try_recv().is_err());
        assert!(bus.drain().next().is_none());

        release.send(()).unwrap();
        assert_eq!(
            reply(&mut bus).await,
            AppEvent::AddSourceBlock {
                snapshot: sample_source_snapshot(),
            }
        );
        assert_eq!(
            timeout(WAIT, entered.recv()).await.unwrap().unwrap(),
            (1, 1)
        );
        release.send(()).unwrap();
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 2);
        assert_eq!(catalog.active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn query_activity_resets_when_a_query_unwinds() {
        let result = std::panic::catch_unwind(|| {
            let _query = SourceQueryActive::enter();
            assert!(SOURCE_QUERY_ACTIVE.get());
            panic!("native enumeration failed");
        });
        assert!(result.is_err());
        assert!(!SOURCE_QUERY_ACTIVE.get());
    }
}
