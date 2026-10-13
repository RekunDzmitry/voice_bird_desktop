//! Observe the actual synchronous native Core ML state initialization.
//!
//! whisper-rs enables native fallback for other workspace consumers. Next must
//! reject fallback rather than claim acceleration just because a feature is on.
use std::cell::Cell;
use std::ffi::CStr;
use std::sync::LazyLock;

use whisper_rs::whisper_rs_sys as sys;

thread_local! {
    // None outside state creation, Some(false) until the native success message.
    static LOADED: Cell<Option<bool>> = const { Cell::new(None) };
}

static LOG_OBSERVER: LazyLock<()> = LazyLock::new(|| {
    // Consume the library's once-only setup first so a later call cannot
    // replace our observer. Preserve its log-crate routing below.
    whisper_rs::install_whisper_log_trampoline();
    // SAFETY: process-lifetime callback, no borrowed userdata, no Rust panics.
    unsafe { sys::whisper_log_set(Some(observe_log), std::ptr::null_mut()) };
});

unsafe extern "C" fn observe_log(
    level: sys::ggml_log_level,
    text: *const std::os::raw::c_char,
    _: *mut std::os::raw::c_void,
) {
    if text.is_null() {
        return;
    }
    // SAFETY: whisper.cpp supplies a NUL-terminated string for this call.
    let text = unsafe { CStr::from_ptr(text) };
    let bytes = text.to_bytes();
    let _ = LOADED.try_with(|loaded| {
        if loaded.get().is_some() {
            if bytes
                .windows(b": Core ML model loaded".len())
                .any(|window| window == b": Core ML model loaded")
            {
                loaded.set(Some(true));
            } else if bytes
                .windows(b": failed to load Core ML model".len())
                .any(|window| window == b": failed to load Core ML model")
            {
                loaded.set(Some(false));
            }
        }
    });
    let message = text.to_string_lossy();
    let message = message.trim();
    match level {
        sys::ggml_log_level_GGML_LOG_LEVEL_DEBUG => log::debug!(target: "whisper_rs", "{message}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_INFO => log::info!(target: "whisper_rs", "{message}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_WARN => log::warn!(target: "whisper_rs", "{message}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_ERROR => log::error!(target: "whisper_rs", "{message}"),
        _ => log::warn!(target: "whisper_rs", "{message}"),
    }
}

struct ObserveInit;

impl Drop for ObserveInit {
    fn drop(&mut self) {
        LOADED.with(|loaded| loaded.set(None));
    }
}

pub(super) fn create_state(
    context: &whisper_rs::WhisperContext,
) -> anyhow::Result<whisper_rs::WhisperState> {
    LazyLock::force(&LOG_OBSERVER);
    LOADED.with(|loaded| loaded.set(Some(false)));
    let _scope = ObserveInit;
    let state = context.create_state()?;
    anyhow::ensure!(
        LOADED.with(|loaded| loaded.get() == Some(true)),
        "required Core ML encoder did not load; native fallback rejected (reinstall the encoder asset or check Core ML diagnostics)"
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(message: &str) {
        let message = std::ffi::CString::new(message).unwrap();
        unsafe {
            observe_log(
                sys::ggml_log_level_GGML_LOG_LEVEL_INFO,
                message.as_ptr(),
                std::ptr::null_mut(),
            )
        };
    }

    #[test]
    fn fallback_and_missing_success_never_mark_a_state_accelerated() {
        LOADED.with(|loaded| loaded.set(Some(false)));
        let _scope = ObserveInit;
        emit("whisper_init_state: loading Core ML model from 'encoder.mlmodelc'\n");
        assert_eq!(LOADED.with(Cell::get), Some(false));
        emit("whisper_init_state: failed to load Core ML model from 'encoder.mlmodelc'\n");
        assert_eq!(LOADED.with(Cell::get), Some(false));
        emit("whisper_init_state: Core ML model loaded\n");
        assert_eq!(LOADED.with(Cell::get), Some(true));
    }

    #[test]
    fn concurrent_initializations_observe_only_their_own_thread() {
        LOADED.with(|loaded| loaded.set(Some(false)));
        let _scope = ObserveInit;
        std::thread::spawn(|| {
            LOADED.with(|loaded| loaded.set(Some(false)));
            let _scope = ObserveInit;
            emit("whisper_init_state: Core ML model loaded\n");
            assert_eq!(LOADED.with(Cell::get), Some(true));
        })
        .join()
        .unwrap();
        assert_eq!(LOADED.with(Cell::get), Some(false));
    }
}
