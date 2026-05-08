# Notes — True application support on macOS (2026-05-05)

Follow-up to the multi-source / per-section streaming work (Stages 1–5). That
work added 3 parallel transcript columns, Tab-based focus cycling, per-source
settings persistence, macOS per-app capture via SCK, and Windows process
loopback resurrected from `b555b95`. After it landed, the macOS app list in
the source picker showed only one entry — "Accessibility" — instead of the
running apps the user expected (Chrome, Slack, Music, Spotify, Zoom, …).

This note records what was changed to make the application list real.

---

## The problem

The previous implementation enumerated apps via
`SCShareableContent::get().applications()`. Two compounding issues:

1. **Apple's contract** for `SCShareableContent.applications` is "running
   applications that have at least one shareable window". Apps in the menu
   bar / tray (Spotify minimized, Music in the background) and audio-only
   helpers are filtered out *by design*. That alone explains a lot of empty
   list.
2. **Screen Recording permission scope.** When the host process (Terminal
   running `cargo run`, or the standalone `voice-bird` binary) lacks the
   "Screen Recording" privilege, `SCShareableContent` collapses to a
   near-empty set — typically just system internal entries like
   "Accessibility". Voice Bird surfaced no UX hint when this happened, so
   the user couldn't distinguish "no apps running" from "permission missing".

The screencapturekit crate's `get()` already uses the most permissive
`Default` capture option (verified in
`screencapturekit-0.3.6/src/shareable_content/mod.rs:86-118`). There is no
looser knob to turn at the SCK enumeration layer.

## The fix

**Enumerate via `NSWorkspace.runningApplications`. Capture via SCK
(unchanged).**

`NSWorkspace.runningApplications` requires no privacy permission and returns
every running NSRunningApplication with `bundleIdentifier`,
`localizedName`, `processIdentifier`, and `activationPolicy`. Filtering to
`activationPolicy == NSApplicationActivationPolicyRegular` (= 0) gives
exactly the user-facing apps the user expects, with daemons and helper
agents excluded.

The capture path is unchanged. When the user picks an app,
`loopback_macos::capture_app(bundle_id)` still queries SCK at capture time
and looks up the `SCRunningApplication` by `bundle_identifier` (with a
`localizedName` fallback). If a NSWorkspace-listed app has no shareable
window — so SCK doesn't see it — the existing
`"application '%s' not found among %d running apps — try refreshing [r]"`
error provides a clear signal.

In addition: a non-prompting Screen Recording permission preflight runs at
launch via `CGPreflightScreenCaptureAccess()`. If the permission is missing,
the existing launch banner reads:

> Screen Recording permission required for system / per-app audio —
> System Settings → Privacy & Security → Screen Recording → enable your
> terminal (or voice-bird), then restart

We deliberately don't auto-prompt with `CGRequestScreenCaptureAccess` —
macOS TCC decisions don't propagate to a running process, so the user must
restart anyway after granting.

## Files changed

| File | Change |
|------|--------|
| `src/audio/loopback/loopback_macos.rs` | Added `screen_recording_permission_granted()` wrapping `CGPreflightScreenCaptureAccess()` via inline `#[link(name = "CoreGraphics", kind = "framework")] extern`. |
| `src/platform/mod.rs` | Replaced macOS arm of `enumerate_app_sessions` with an `NSWorkspace.runningApplications` walk via raw `objc` `msg_send!`. Filters to regular activation policy, dedups by bundle id (or localized name when bundle id is empty), sorts alphabetically. |
| `src/app.rs` (`App::new`) | On macOS, when `screen_recording_permission_granted()` returns false, sets the launch banner to the remediation message above. Overrides the cloud-key warning since the missing-permission case is the more critical blocker. |
| `Cargo.toml` | Added `objc = "0.2"` under `[target.'cfg(target_os = "macos")'.dependencies]`. The crate was already present transitively via `screencapturekit` 0.3.6; this just makes it nameable from our own code. No other deps added. |

The capture path (`loopback_macos::capture_app`) is unchanged.
The Windows path is unchanged — its WASAPI session-manager enumeration is
already an audio-producing-process model, which is the right model.

## Why NSWorkspace and not …

- **`NSRunningApplication` directly via `objc2-app-kit`** — would have added
  a heavy new dep family for a single small enumeration. The existing
  `objc` 0.2 macros (already used internally by screencapturekit) cover the
  five `msg_send!`s we need.
- **CoreAudio's `kAudioHardwarePropertyProcessIsAudible`** — would let us
  show only currently-audible apps. Useful, but the user's first ask is
  visibility (show every app), not filtering. That can land later as an
  optional toggle.
- **Polling SCShareableContent more aggressively / different SCK options** —
  no looser knob exists; "Default" is already the most permissive
  capture-option variant. The window-bearing-app filter is hard-coded into
  the API.

## Verification

End-to-end:

1. `cargo build` — clean (no new errors; pre-existing dead-code warnings
   only).
2. `cargo test --lib --bins --tests` — all suites green except the
   pre-existing `voicebird_engine::shutdown_sends_terminate_text_message`
   timing flake (unrelated to this work; touches WebSocket termination
   timing only).
3. Manual happy path with permission granted: `cargo run`, observe the
   source list contains every regular running app — Chrome, Music, Slack,
   Spotify, Zoom, Cursor, Finder, … Pick one, press Enter, confirm capture
   starts and the column shows transcript activity.
4. Manual missing-permission path: revoke Screen Recording for the
   terminal in System Settings, restart `cargo run`. Banner reads
   "Screen Recording permission required …". App list is still populated
   (proving NSWorkspace doesn't depend on TCC), but starting any
   `[output/loopback]` or `[app]` section fails with a clear permission
   error from the existing capture entry points.
5. Manual refresh: launch a new app after voice-bird is running, press
   `r`, confirm it joins the list.

## Out of scope for this change

- Filtering to currently-audible apps (a "show only audio-producing apps"
  toggle). Could land via CoreAudio
  `kAudioHardwarePropertyProcessIsAudible` polling.
- Windows app-list improvements. WASAPI session enumeration already
  produces the audio-producing-process model.
- Auto-prompting for Screen Recording permission. macOS TCC requires a
  restart after grant; the banner is the standard pattern (used by OBS,
  Loom, etc.).
