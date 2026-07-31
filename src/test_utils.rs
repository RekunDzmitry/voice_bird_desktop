//! Test-only helpers shared between the binary's `app::tests` and
//! `main::tests` modules.
//!
//! The headline helper is `install_test_config` — a
//! `LazyLock<PathBuf>` that, on first deref, points the in-process
//! config loader at a tempdir via the `VOICE_BIRD_TEST_CONFIG_PATH`
//! env var. After installation every `App::new()` in this test
//! binary loads from and saves to the tempdir, so the test suite
//! never reads the developer's real `config.toml` and never
//! writes the in-memory `sk-test` key / flipped cloud flag back
//! over it.
//!
//! Also un-flakes the two banner tests
//! (`help_and_status_keys_toggle_independent_modes`,
//! `question_mark_is_help_only_and_t_owns_status`) that asserted
//! `app.banner.is_none()` — they failed when the developer's real
//! `cloud_broadcast_enabled = true` + `voicebird_api_key = ""`
//! triggered the on-launch banner. With the tempdir the App
//! boots from `AppConfig::default()` and `banner_on_launch` is
//! `None` (cloud is off by default).
//!
//! Lazy init means: tests that don't touch `App::new()` (pure
//! formatting / unit tests) skip the tempdir dance entirely.
//!
//! ## How to install
//!
//! The canonical pattern is a module-level `LazyLock<()>` whose
//! init is just a deref of `install_test_config`:
//!
//! ```ignore
//! static _TEST_CONFIG: LazyLock<()> = LazyLock::new(|| {
//!     let _ = &*test_utils::install_test_config();
//! });
//! ```
//!
//! Why a `LazyLock<()>` rather than a plain `static X = T;`:
//! the `LazyLock` is a non-ZST type with a real initializer, so
//! the compiler can't elide it as "unused" before any test
//! runs. Cargo's test harness touches the `LazyLock` (it's
//! `static` at module scope) and the init runs before the first
//! `App::new()` in that module's tests. The `static _TEST_CONFIG`
//! has type `()` so it costs nothing past init; the real
//! allocation lives in `install_test_config`.

use std::path::PathBuf;
use std::sync::LazyLock;

/// Process-wide handle to the per-test-binary config dir.
///
/// On first access this allocates a fresh tempdir (under
/// `std::env::temp_dir()`, with a `voice-bird-tests-<pid>-<nanos>`
/// suffix), sets `VOICE_BIRD_TEST_CONFIG_PATH` to a `config.toml`
/// inside it, and returns the path. The tempdir is intentionally
/// NOT deleted on drop — leaving it behind makes post-mortem
/// inspection easy (`ls $(cat /tmp/voice-bird-tests-…)`) and the
/// test process exits anyway, so the OS reclaims it.
///
/// The `LazyLock` ensures single init per process, thread-safe.
/// Every test sees the SAME tempdir, so they share state
pub static INSTALL_TEST_CONFIG: LazyLock<PathBuf> = LazyLock::new(|| {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "voice-bird-tests-pid{pid}-n{nanos}-c{n}"
    ));
    std::fs::create_dir_all(&dir).expect("create test config tempdir");
    let path = dir.join("config.toml");
    // `set_var` is `unsafe` from Rust 2024; the test binary is on
    // 2021 so it's still safe. We do this exactly once per
    // process, before any test starts, so no other thread is
    // racing on the env.
    std::env::set_var("VOICE_BIRD_TEST_CONFIG_PATH", &path);
    path
});

/// A fresh, unshared `config.toml` path under a new tempdir.
///
/// `App::new()` uses this in test builds so every constructed `App`
/// starts from its own empty config. The shared
/// [`INSTALL_TEST_CONFIG`] tempdir keeps the developer's real config
/// safe, but it is one file for the whole binary — once `App` began
/// persisting the slot layout (`add_slot` / `remove_focused_slot`),
/// one test's slots leaked into the next test's `App::new()` and the
/// resulting slot ids depended on test execution order.
///
/// Like the shared tempdir, these are intentionally not deleted:
/// the process exits and the OS reclaims them.
pub fn fresh_test_config_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("voice-bird-app-pid{pid}-n{nanos}-c{n}"));
    std::fs::create_dir_all(&dir).expect("create per-App test config tempdir");
    dir.join("config.toml")
}

/// The real `config.toml` path, IGNORING the
/// `VOICE_BIRD_TEST_CONFIG_PATH` env var. Used by tests that
/// want to verify "did the production code touch the
/// developer's real config?" — those tests must look at the
/// real path, not whatever the in-process override points
/// at. Replicates the production calculation
/// (`<dirs::config_dir>/voice-bird/config.toml`).
pub fn real_config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .expect("dirs::config_dir available")
        .join("voice-bird")
        .join("config.toml")
}
