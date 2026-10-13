//! On-disk transcription-model store (Whisper GGUF/Core ML, Nemotron ONNX).
//!
//! The download pipeline is format-agnostic: fetch the URL to a staging
//! file, verify its sha256, hand the staged file to a format-specific
//! `install`. Only [`ModelFormatHandler::install`] and
//! [`ModelFormatHandler::is_installed`] know what the bytes actually
//! are — adding a new format is one handler impl plus one arm in
//! [`handler_for`].
//!
//! Staging + install is what keeps the presence check honest: a
//! truncated or sha-failing write that went straight to the final path
//! would render as "present" the next time `is_available` runs.
//!
//! ## Attempt-scoped staging paths
//!
//! `ModelFormatHandler::staging_path` takes an `attempt: u32`. The
//! table picks a fresh attempt id every time a request returns
//! `Restart` (after a `Cancelling` row has not yet been acked). The
//! handlers suffix the attempt into the staging filename so two
//! concurrent attempts of the same model cannot write to the same
//! `.part` file or unpack into the same scratch directory. The table
//! filters every event by attempt id, so a stale `DownloadInstalling`
//! from a superseded attempt cannot repaint the new attempt's
//! progress row either.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;

use crate::db::downloads::CancelCheck;
use crate::download::DownloadError;
use crate::picker::{ModelEntry, ModelFormat};

/// Everything format-specific about getting a model onto disk and
/// deciding whether it is already there.
pub trait ModelFormatHandler: Send + Sync {
    /// Resolve the on-disk staging path for `(dir, id, attempt)`.
    /// `attempt` is the `Claim`'s monotonic id, supplied by the
    /// table so two concurrent attempts of the same model cannot
    /// write to the same staging file. Each handler picks its
    /// own attempt-suffixing scheme; the table never inspects the
    /// path.
    fn staging_path(&self, dir: &Path, id: &str, attempt: u32) -> PathBuf;
    /// Final installed path. Stable across attempts: this is the path
    /// the renderer reports as "available" once install finishes, and
    /// it does NOT carry an attempt suffix.
    fn installed_path(&self, dir: &Path, id: &str) -> PathBuf;
    /// Present *and usable* — not merely "a path exists".
    fn is_installed(&self, dir: &Path, id: &str) -> bool;
    /// Move/unpack the verified staging file into place. The cancel
    /// check is the same [`crate::db::downloads::CancelProbe`] the
    /// worker polled during fetch; it is observed between unpack
    /// and rename so closing the last waiting block during the
    /// install phase actually stops the worker instead of leaving a
    /// half-unpacked directory on disk. Returning
    /// `DownloadError::Cancelled` is the contract the worker relies
    /// on to publish nothing — the producer already flipped the row
    /// to `Cancelling` when it set the cancel state.
    fn install(
        &self,
        dir: &Path,
        id: &str,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError>;

    /// Whether `install` is slow enough to deserve its own UI phase
    /// (unpacking 740 MB is; a rename is not).
    fn install_is_slow(&self) -> bool {
        false
    }
}

pub fn handler_for(format: ModelFormat) -> &'static dyn ModelFormatHandler {
    match format {
        ModelFormat::WhisperGguf => &GgufHandler,
        ModelFormat::NemotronPackage => &NemotronPackageHandler,
        ModelFormat::CoreMlEncoder => &CoreMlEncoderHandler,
    }
}

/// Single Whisper GGUF `.bin` file. Staging filename
/// `<id>.<attempt>.gguf.part`, installed `<id>.gguf` (matches the
/// legacy crate's `gguf_path` so a model already fetched by
/// `voice-bird-cli` reads as present).
///
/// The attempt suffix on the staging filename is what keeps two
/// concurrent attempts of the same model from racing on the same
/// `.part` file: even if the table issues `Restart` under a new
/// attempt while the previous worker is still alive, the in-flight
/// GGUF rename for the previous attempt operates on a different
/// filename. Without the suffix, the rename would have promoted a
/// half-written `.part` from the new attempt into the installed
/// `.gguf`.
pub struct GgufHandler;

impl ModelFormatHandler for GgufHandler {
    fn staging_path(&self, dir: &Path, id: &str, attempt: u32) -> PathBuf {
        dir.join(format!("{id}.{attempt}.gguf.part"))
    }
    fn installed_path(&self, dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.gguf"))
    }
    fn is_installed(&self, dir: &Path, id: &str) -> bool {
        self.installed_path(dir, id).is_file()
    }
    fn install(
        &self,
        dir: &Path,
        id: &str,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        // GGUF install is a single atomic rename; the cancel check
        // can only be observed once the worker re-enters its outer
        // loop, so it has no useful check point here. The handler
        // still takes the parameter so the trait stays uniform
        // across formats — a future handler that streams entries
        // can poll the token between iterations.
        let _ = cancel;
        let target = self.installed_path(dir, id);
        fs::rename(staged, &target).map_err(|e| DownloadError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Nemotron ONNX package, shipped as a `.tar.gz`. Staging filename
/// `<id>.<attempt>.tar.gz.part`, unpack scratch
/// `<id>.<attempt>.tmp/`, installed `<id>/` (matches the legacy
/// `nemotron_model_dir`).
///
/// Both the staging archive and the unpack scratch carry the
/// attempt suffix so two concurrent attempts cannot collide on
/// either path. Without the suffix, the previous design would
/// have let a retry's `tmp_dir` cleanup (`remove_dir_all` in the
/// install prologue) wipe a still-running previous attempt's
/// partial unpack.
///
/// `is_installed` requires **both** `encoder.onnx` and
/// `decoder_joint.onnx` so an empty or half-unpacked directory
/// is not mistaken for a model.
pub struct NemotronPackageHandler;

impl ModelFormatHandler for NemotronPackageHandler {
    fn staging_path(&self, dir: &Path, id: &str, attempt: u32) -> PathBuf {
        dir.join(format!("{id}.{attempt}.tar.gz.part"))
    }
    fn installed_path(&self, dir: &Path, id: &str) -> PathBuf {
        dir.join(id)
    }
    fn is_installed(&self, dir: &Path, id: &str) -> bool {
        let path = self.installed_path(dir, id);
        path.is_dir()
            && path.join("encoder.onnx").is_file()
            && path.join("decoder_joint.onnx").is_file()
    }
    fn install(
        &self,
        dir: &Path,
        id: &str,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        // The unpack scratch dir mirrors the staging file's attempt
        // suffix by stripping the trailing `.part` off `staged`'s
        // filename and appending `.tmp`. Deriving it from `staged`
        // (rather than from `id` and a fixed suffix) means the
        // scratch dir always agrees with whatever staging filename
        // the worker fetched into, and gives us per-attempt
        // isolation for free.
        let tmp_dir = match Path::new(staged).file_name().and_then(|n| n.to_str()) {
            Some(name) => dir.join(format!("{}.tmp", name.trim_end_matches(".part"))),
            None => {
                return Err(DownloadError::Io(
                    "staged file has no name component".into(),
                ));
            }
        };
        let target = self.installed_path(dir, id);
        // Best-effort cleanup of leftovers from a prior crashed run.
        if tmp_dir.exists() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }
        if target.exists() {
            let _ = fs::remove_dir_all(&target);
        }
        fs::create_dir_all(&tmp_dir).map_err(|e| DownloadError::Io(e.to_string()))?;
        // Unpack entry by entry so the cancel check can be polled
        // between entries. `Archive::unpack` would have run straight
        // through with no observation point, which is exactly the
        // bug the cancel-during-install repro exposed.
        let result = (|| -> Result<(), DownloadError> {
            let file = fs::File::open(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
            let decoder = GzDecoder::new(file);
            let mut archive = Archive::new(decoder);
            let entries = archive
                .entries()
                .map_err(|e| DownloadError::Install(format!("unpack: {e}")))?;
            for entry in entries {
                if cancel.is_cancelled() {
                    // The producer already flipped the row to
                    // Cancelling when it set the token; the worker
                    // treats this the same as the fetch-time
                    // Cancelled and publishes nothing.
                    return Err(DownloadError::Cancelled);
                }
                let mut entry =
                    entry.map_err(|e| DownloadError::Install(format!("unpack: {e}")))?;
                let path = entry
                    .path()
                    .map_err(|e| DownloadError::Install(format!("unpack: {e}")))?
                    .into_owned();
                // Per-entry `unpack_in` does not auto-create the
                // parent directory the way `Archive::unpack` does.
                // Recreate it manually so the manual loop matches
                // the batch call's behavior. Root-level entries
                // (parent == tmp_dir) already exist.
                let parent = path.parent().unwrap_or(&tmp_dir);
                let full_parent = tmp_dir.join(parent);
                if full_parent != tmp_dir {
                    fs::create_dir_all(&full_parent)
                        .map_err(|e| DownloadError::Install(format!("unpack: {e}")))?;
                }
                entry
                    .unpack_in(&tmp_dir)
                    .map_err(|e| DownloadError::Install(format!("unpack: {e}")))?;
            }
            let model_dir = locate_nemotron_dir(&tmp_dir).ok_or_else(|| {
                DownloadError::Install(
                    "Nemotron package did not contain encoder.onnx and decoder_joint.onnx".into(),
                )
            })?;
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            fs::rename(model_dir, &target).map_err(|e| DownloadError::Io(e.to_string()))?;
            Ok(())
        })();
        let _ = fs::remove_dir_all(&tmp_dir);
        match &result {
            // Install succeeded: drop the source archive now that
            // every byte has been verified and the rename moved the
            // model into place.
            Ok(()) => {
                let _ = fs::remove_file(staged);
            }
            // Cancelled between download and unpack: the user
            // explicitly aborted, so drop the staged archive too.
            // The next pick will re-fetch from scratch instead of
            // resuming from a half-valid tarball.
            Err(DownloadError::Cancelled) => {
                let _ = fs::remove_file(staged);
            }
            // Any other failure (corrupt tar, missing ONNX pair,
            // IO error) keeps the staged file: the SHA was verified
            // at fetch time and a Retry can install it without
            // re-downloading the 740 MB.
            Err(_) => {}
        }
        result
    }

    fn install_is_slow(&self) -> bool {
        true
    }
}

/// Compiled Whisper encoder ZIP. The catalog id is the runtime's exact stem
/// plus `-encoder`; official archive roots retain their `ggml-` prefix.
pub struct CoreMlEncoderHandler;

/// A directory alone cannot indicate readiness. These are the executable
/// bundle artifacts in the pinned official encoder archives. Native state
/// creation additionally verifies that Core ML can actually load the bundle.
pub(crate) fn coreml_bundle_present(path: &Path) -> bool {
    let directory = |path: &Path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
    };
    let file = |path: &Path| {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() > 0)
    };
    directory(path)
        && directory(&path.join("weights"))
        && file(&path.join("coremldata.bin"))
        && file(&path.join("model.mil"))
        && file(&path.join("metadata.json"))
        && file(&path.join("weights/weight.bin"))
}

impl ModelFormatHandler for CoreMlEncoderHandler {
    fn staging_path(&self, dir: &Path, id: &str, attempt: u32) -> PathBuf {
        dir.join(format!("{id}.{attempt}.mlmodelc.zip.part"))
    }

    fn installed_path(&self, dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.mlmodelc"))
    }

    fn is_installed(&self, dir: &Path, id: &str) -> bool {
        coreml_bundle_present(&self.installed_path(dir, id))
    }

    fn install(
        &self,
        dir: &Path,
        id: &str,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        let archive_root = match id {
            "distil-small.en-encoder" => "ggml-small.en-encoder.mlmodelc",
            "large-v3-turbo-encoder" => "ggml-large-v3-turbo-encoder.mlmodelc",
            _ => {
                return Err(DownloadError::Install(format!(
                    "unknown Core ML encoder {id}"
                )))
            }
        };
        let name = staged
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| DownloadError::Io("staged file has no name component".into()))?;
        let scratch = dir.join(format!("{}.tmp", name.trim_end_matches(".part")));
        if scratch.exists() {
            fs::remove_dir_all(&scratch).map_err(|error| DownloadError::Io(error.to_string()))?;
        }
        fs::create_dir_all(&scratch).map_err(|error| DownloadError::Io(error.to_string()))?;
        let result = (|| {
            let source =
                fs::File::open(staged).map_err(|error| DownloadError::Io(error.to_string()))?;
            let mut archive = zip::ZipArchive::new(source)
                .map_err(|error| DownloadError::Install(format!("encoder ZIP: {error}")))?;
            let mut buffer = [0u8; 64 * 1024];
            for index in 0..archive.len() {
                if cancel.is_cancelled() {
                    return Err(DownloadError::Cancelled);
                }
                let mut entry = archive.by_index(index).map_err(|error| {
                    DownloadError::Install(format!("encoder ZIP entry: {error}"))
                })?;
                let relative = entry
                    .enclosed_name()
                    .ok_or_else(|| DownloadError::Install("unsafe encoder ZIP path".into()))?;
                if relative
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err(DownloadError::Install("unsafe encoder ZIP path".into()));
                }
                if let Some(mode) = entry.unix_mode() {
                    let kind = mode & 0o170000;
                    if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                        return Err(DownloadError::Install(
                            "encoder ZIP contains a link or special file".into(),
                        ));
                    }
                }
                // ZIPs may also contain packaging metadata. Install only the
                // expected bundle subtree, never unrelated archive contents.
                if !relative.starts_with(archive_root) {
                    continue;
                }
                let destination = scratch.join(relative);
                if entry.is_dir() {
                    fs::create_dir_all(&destination)
                        .map_err(|error| DownloadError::Io(error.to_string()))?;
                } else {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|error| DownloadError::Io(error.to_string()))?;
                    }
                    // Reject duplicate entries rather than silently overwriting.
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(destination)
                        .map_err(|error| DownloadError::Io(error.to_string()))?;
                    loop {
                        if cancel.is_cancelled() {
                            return Err(DownloadError::Cancelled);
                        }
                        let count = entry.read(&mut buffer).map_err(|error| {
                            DownloadError::Install(format!("encoder ZIP data: {error}"))
                        })?;
                        if count == 0 {
                            break;
                        }
                        file.write_all(&buffer[..count])
                            .map_err(|error| DownloadError::Io(error.to_string()))?;
                    }
                }
            }
            let bundle = scratch.join(archive_root);
            if !coreml_bundle_present(&bundle) {
                return Err(DownloadError::Install(
                    "encoder ZIP contains an incomplete Core ML bundle".into(),
                ));
            }
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            let target = self.installed_path(dir, id);
            if !coreml_bundle_present(&target) {
                if target.exists() {
                    fs::remove_dir_all(&target)
                        .map_err(|error| DownloadError::Io(error.to_string()))?;
                }
                fs::rename(bundle, target).map_err(|error| DownloadError::Io(error.to_string()))?;
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&scratch);
        if result.is_ok() || matches!(&result, Err(DownloadError::Cancelled)) {
            let _ = fs::remove_file(staged);
        }
        // Other errors retain the checksummed archive for an install-only retry.
        result
    }

    fn install_is_slow(&self) -> bool {
        true
    }
}

/// Locate the directory inside `root` that holds both ONNX artifacts.
/// `read_dir` may legitimately fail on a subdirectory we don't have
/// permission to list (sandboxed installs, read-only mounts); the
/// legacy version propagated that error and reported "package did not
/// contain encoder.onnx" — the plan catches this with `continue`.
///
/// Search is iterative so we don't blow the stack on a deeply nested
/// tarball.
fn locate_nemotron_dir(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("encoder.onnx").is_file() && dir.join("decoder_joint.onnx").is_file() {
            return Some(dir);
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    None
}

/// Model-store surface used by the watcher and model-store consumer.
/// `CacheDirStore` delegates format-specific work to [`ModelFormatHandler`].
pub trait ModelStore: Send + Sync + 'static {
    fn is_available(&self, entry: &ModelEntry) -> bool;
    /// Resolve the installed artifact used when opening a local engine.
    fn installed_path(&self, entry: &ModelEntry) -> PathBuf;
    /// Resolve the staging path for the supplied attempt id.
    /// The watcher and model-store consumer persist it in SQLite before fetch;
    /// they thread the claimed attempt through to the handler so concurrent
    /// attempts of the same model never share a `.part` file.
    fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError>;
    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError>;
    /// Drop BOTH the staged archive and the unpack scratch directory
    /// for one model, regardless of which phase the worker is in.
    /// Called at Quit for every in-flight model so the cache dir
    /// is left clean for the next session — without this the next
    /// session's first pick on the same model would attempt to
    /// unpack a half-written `.tmp/` left by the killed worker.
    fn discard_inflight(&self, entry: &ModelEntry);
}

/// Resolves to `<cache_dir>/voice-bird/models/`. Kept identical to the
/// legacy crate's `dirs::cache_dir()` path so a model downloaded by
/// `voice-bird-cli` reads as present.
pub struct CacheDirStore {
    root: PathBuf,
}

impl CacheDirStore {
    pub fn new() -> Result<Self, DownloadError> {
        let root = models_dir()?;
        fs::create_dir_all(&root).map_err(|e| DownloadError::Io(e.to_string()))?;
        Ok(Self { root })
    }
    /// Build a store at an explicit models root.
    ///
    /// Production uses [`Self::new`]; callers that need an isolated persistent
    /// cache can provide a temporary root and reopen it across sessions.
    pub fn from_root(root: PathBuf) -> Result<Self, DownloadError> {
        fs::create_dir_all(&root).map_err(|error| DownloadError::Io(error.to_string()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Delete every `*.part` file and `*.tmp` directory left by a
    /// previous run. Called once from `main`. Staging names are
    /// deterministic per `(id, attempt)` so a restarted download
    /// would overwrite rather than accumulate; the sweep is what
    /// stops any leftover `.tmp/` directories from a killed worker
    /// from sitting on disk forever. Newer attempts use a higher
    /// attempt number, so an old attempt's leftover is matched by
    /// the same wildcard.
    pub fn sweep_staging(&self) -> Result<(), DownloadError> {
        if !self.root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.root).map_err(|e| DownloadError::Io(e.to_string()))? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Match the previous suffix patterns too. The current
            // attempt-scoped names use `<id>.<attempt>.gguf.part`
            // and `<id>.<attempt>.tar.gz.part`, both of which still
            // end in `.part`; scratch dirs end in `.tmp`. We don't
            // try to be picky about the prefix.
            if name.ends_with(".part") && path.is_file() {
                let _ = fs::remove_file(&path);
            } else if name.ends_with(".tmp") && path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            }
        }
        Ok(())
    }
}

impl ModelStore for CacheDirStore {
    fn is_available(&self, entry: &ModelEntry) -> bool {
        handler_for(entry.format).is_installed(&self.root, entry.id)
    }

    fn installed_path(&self, entry: &ModelEntry) -> PathBuf {
        handler_for(entry.format).installed_path(&self.root, entry.id)
    }

    fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError> {
        Ok(handler_for(entry.format).staging_path(&self.root, entry.id, attempt))
    }

    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        handler_for(entry.format).install(&self.root, entry.id, staged, cancel)
    }

    fn discard_inflight(&self, _entry: &ModelEntry) {
        // Both startup and quit cleanup sweep every attempt's staging artifacts,
        // including archives and unpack scratch directories, but never installed
        // model files.
        let _ = self.sweep_staging();
    }
}

/// Resolve the cache directory the same way the legacy crate did:
/// `<os cache_dir>/voice-bird/models`. Pinning this suffix in a test
/// (below) means a future drift between `next` and `voice-bird-cli`
/// fails CI rather than silently splitting the cache.
pub fn models_dir() -> Result<PathBuf, DownloadError> {
    let base = directories::BaseDirs::new()
        .ok_or(DownloadError::NoCacheDir)?
        .cache_dir()
        .to_path_buf();
    Ok(base.join("voice-bird").join("models"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::CATALOG;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn nemotron_entry() -> &'static ModelEntry {
        &CATALOG[3]
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn encoder_zip(path: &Path, files: &[&str]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for name in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"compiled artifact").unwrap();
        }
        zip.finish().unwrap();
    }

    const ENCODER_FILES: &[&str] = &[
        "ggml-small.en-encoder.mlmodelc/coremldata.bin",
        "ggml-small.en-encoder.mlmodelc/model.mil",
        "ggml-small.en-encoder.mlmodelc/metadata.json",
        "ggml-small.en-encoder.mlmodelc/weights/weight.bin",
    ];

    #[test]
    fn encoder_install_uses_runtime_stem_and_only_complete_bundles_are_ready() {
        let tmp = TempDir::new().unwrap();
        let entry = &crate::picker::DISTIL_SMALL_EN_ENCODER;
        let store = CacheDirStore::from_root(tmp.path().to_owned()).unwrap();
        let staged = store.staging_path(entry, 7).unwrap();
        encoder_zip(&staged, ENCODER_FILES);
        let cancel = AtomicBool::new(false);
        store.install(entry, &staged, &mut { &cancel }).unwrap();
        let bundle = store.installed_path(entry);
        assert_eq!(bundle, tmp.path().join("distil-small.en-encoder.mlmodelc"));
        assert!(store.is_available(entry));
        fs::remove_file(bundle.join("weights/weight.bin")).unwrap();
        assert!(!store.is_available(entry));
        assert!(!staged.exists());
        assert!(!tmp
            .path()
            .join("distil-small.en-encoder.7.mlmodelc.zip.tmp")
            .exists());
    }

    #[test]
    fn encoder_install_accepts_packaging_metadata_without_installing_it() {
        let tmp = TempDir::new().unwrap();
        let entry = &crate::picker::DISTIL_SMALL_EN_ENCODER;
        let store = CacheDirStore::from_root(tmp.path().to_owned()).unwrap();
        let staged = store.staging_path(entry, 1).unwrap();
        let mut files = ENCODER_FILES.to_vec();
        files.extend([
            "__MACOSX/._ggml-small.en-encoder.mlmodelc",
            "release-notes.txt",
        ]);
        encoder_zip(&staged, &files);
        let cancel = AtomicBool::new(false);
        store.install(entry, &staged, &mut { &cancel }).unwrap();
        assert!(store.is_available(entry));
        assert!(!tmp.path().join("__MACOSX").exists());
        assert!(!tmp.path().join("release-notes.txt").exists());
    }

    #[test]
    fn unsafe_and_incomplete_encoder_archives_cannot_replace_a_ready_bundle() {
        let tmp = TempDir::new().unwrap();
        let h = CoreMlEncoderHandler;
        let id = crate::picker::DISTIL_SMALL_EN_ENCODER.id;
        let cancel = AtomicBool::new(false);
        let staged = h.staging_path(tmp.path(), id, 1);
        encoder_zip(&staged, ENCODER_FILES);
        h.install(tmp.path(), id, &staged, &mut { &cancel })
            .unwrap();
        for (index, files) in [
            vec!["../escaped"],
            vec!["ggml-small.en-encoder.mlmodelc/../../escaped"],
            vec!["wrong-encoder.mlmodelc/coremldata.bin"],
            vec![ENCODER_FILES[0]],
        ]
        .iter()
        .enumerate()
        {
            let staged = h.staging_path(tmp.path(), id, index as u32 + 2);
            encoder_zip(&staged, files);
            assert!(h
                .install(tmp.path(), id, &staged, &mut { &cancel })
                .is_err());
            assert!(h.is_installed(tmp.path(), id));
            assert!(
                staged.exists(),
                "install-only retry retains its verified archive"
            );
            assert!(!tmp.path().join("escaped").exists());
        }
    }

    #[test]
    fn encoder_cancellation_discards_only_its_attempt_and_allows_recovery() {
        let tmp = TempDir::new().unwrap();
        let h = CoreMlEncoderHandler;
        let id = crate::picker::DISTIL_SMALL_EN_ENCODER.id;
        let first = h.staging_path(tmp.path(), id, 1);
        let second = h.staging_path(tmp.path(), id, 2);
        encoder_zip(&first, ENCODER_FILES);
        encoder_zip(&second, ENCODER_FILES);
        struct CancelDuringCopy(usize);
        impl CancelCheck for CancelDuringCopy {
            fn is_cancelled(&mut self) -> bool {
                self.0 += 1;
                self.0 == 3
            }
        }
        let mut cancelled = CancelDuringCopy(0);
        assert!(matches!(
            h.install(tmp.path(), id, &first, &mut cancelled),
            Err(DownloadError::Cancelled)
        ));
        assert!(!first.exists());
        assert!(!tmp
            .path()
            .join("distil-small.en-encoder.1.mlmodelc.zip.tmp")
            .exists());
        assert!(second.exists());
        assert!(!h.is_installed(tmp.path(), id));
        let active = AtomicBool::new(false);
        h.install(tmp.path(), id, &second, &mut { &active })
            .unwrap();
        assert!(h.is_installed(tmp.path(), id));
    }

    #[test]
    fn encoder_zip_symlinks_are_rejected_without_following_targets() {
        let tmp = TempDir::new().unwrap();
        let h = CoreMlEncoderHandler;
        let id = crate::picker::DISTIL_SMALL_EN_ENCODER.id;
        let staged = h.staging_path(tmp.path(), id, 1);
        let mut zip = zip::ZipWriter::new(fs::File::create(&staged).unwrap());
        zip.add_symlink(
            "ggml-small.en-encoder.mlmodelc/weights",
            "../outside",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.finish().unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            h.install(tmp.path(), id, &staged, &mut { &cancel }),
            Err(DownloadError::Install(_))
        ));
        assert!(!h.installed_path(tmp.path(), id).exists());
        assert!(!tmp.path().join("outside").exists());
    }

    #[test]
    fn models_dir_ends_with_voice_bird_models() {
        let dir = models_dir().unwrap();
        assert!(dir.ends_with("voice-bird/models"), "got {dir:?}");
    }

    #[test]
    fn gguf_is_installed_only_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let h = GgufHandler;
        assert!(!h.is_installed(tmp.path(), "tiny.en"));
        write(&h.installed_path(tmp.path(), "tiny.en"), b"GGUF");
        assert!(h.is_installed(tmp.path(), "tiny.en"));
    }

    #[test]
    fn gguf_staging_path_includes_attempt_suffix() {
        let tmp = TempDir::new().unwrap();
        let h = GgufHandler;
        let p1 = h.staging_path(tmp.path(), "tiny.en", 1);
        let p2 = h.staging_path(tmp.path(), "tiny.en", 2);
        assert_eq!(p1.file_name().unwrap(), "tiny.en.1.gguf.part");
        assert_eq!(p2.file_name().unwrap(), "tiny.en.2.gguf.part");
        assert_ne!(p1, p2, "two attempts must not share a staging path");
    }

    #[test]
    fn gguf_install_renames_staged_file() {
        let tmp = TempDir::new().unwrap();
        let h = GgufHandler;
        let staged = h.staging_path(tmp.path(), "tiny.en", 1);
        write(&staged, b"GGUF");
        let cancel = AtomicBool::new(false);
        h.install(tmp.path(), "tiny.en", &staged, &mut { &cancel })
            .unwrap();
        assert!(!staged.exists());
        assert!(h.installed_path(tmp.path(), "tiny.en").is_file());
    }

    #[test]
    fn nemotron_is_installed_requires_both_onnx_files() {
        let tmp = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        let dir = h.installed_path(tmp.path(), nemotron_entry().id);
        fs::create_dir_all(&dir).unwrap();
        // Only encoder — still missing.
        write(&dir.join("encoder.onnx"), b"e");
        assert!(!h.is_installed(tmp.path(), nemotron_entry().id));
        write(&dir.join("decoder_joint.onnx"), b"d");
        assert!(h.is_installed(tmp.path(), nemotron_entry().id));
    }

    #[test]
    fn nemotron_staging_path_includes_attempt_suffix() {
        let tmp = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        let p1 = h.staging_path(tmp.path(), nemotron_entry().id, 1);
        let p2 = h.staging_path(tmp.path(), nemotron_entry().id, 2);
        assert_eq!(
            p1.file_name().unwrap().to_str().unwrap(),
            format!("{}.1.tar.gz.part", nemotron_entry().id)
        );
        assert_eq!(
            p2.file_name().unwrap().to_str().unwrap(),
            format!("{}.2.tar.gz.part", nemotron_entry().id)
        );
        assert_ne!(p1, p2, "two attempts must not share a staging path");
    }

    #[test]
    fn nemotron_install_unpacks_and_renames() {
        // Build a minimal tar.gz: a directory `pkg/` containing both
        // ONNX files. Run install on it and assert the installed dir
        // carries the artifacts at the right path.
        let src = TempDir::new().unwrap();
        let pkg = src.path().join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        write(&pkg.join("encoder.onnx"), b"e");
        write(&pkg.join("decoder_joint.onnx"), b"d");

        let archive = src
            .path()
            .join(format!("{}.1.tar.gz.part", nemotron_entry().id));
        let f = fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all("pkg", &pkg).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let dst = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        let cancel = AtomicBool::new(false);
        h.install(dst.path(), nemotron_entry().id, &archive, &mut { &cancel })
            .unwrap();
        let installed = h.installed_path(dst.path(), nemotron_entry().id);
        assert!(installed.is_dir());
        assert!(installed.join("encoder.onnx").is_file());
        assert!(installed.join("decoder_joint.onnx").is_file());
        // The attempt-scoped scratch dir was removed by the
        // post-install cleanup.
        assert!(!dst
            .path()
            .join(format!("{}.1.tar.gz.tmp", nemotron_entry().id))
            .exists());
    }

    #[test]
    fn nemotron_install_failure_leaves_no_installed_dir() {
        // Corrupt the archive by truncating; install must roll back
        // the scratch directory AND not leave anything at installed_path.
        let archive_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(archive_dir.path()).unwrap();
        let archive = archive_dir
            .path()
            .join(format!("{}.1.tar.gz.part", nemotron_entry().id));
        write(&archive, b"not a tarball");

        let dst = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        let cancel = AtomicBool::new(false);
        let res = h.install(dst.path(), nemotron_entry().id, &archive, &mut { &cancel });
        assert!(res.is_err());
        assert!(!h.installed_path(dst.path(), nemotron_entry().id).exists());
        assert!(!dst
            .path()
            .join(format!("{}.1.tar.gz.tmp", nemotron_entry().id))
            .exists());
    }

    #[test]
    fn nemotron_locate_skips_unreadable_subdirs() {
        // Force a subdirectory that read_dir can't list by replacing
        // it with a symlink to nothing — the locate function must
        // continue rather than report "package did not contain
        // encoder.onnx". The artifact dir with both files is a
        // sibling, not under the broken link.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let good = root.join("good");
        fs::create_dir_all(&good).unwrap();
        write(&good.join("encoder.onnx"), b"e");
        write(&good.join("decoder_joint.onnx"), b"d");
        // Make an unreadable sibling by removing read permission on
        // macOS this is best-effort; if chmod fails we still want
        // the good sibling to be found, so this test asserts
        // correctness on the happy path regardless.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bad = root.join("bad");
            fs::create_dir(&bad).unwrap();
            fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        }
        assert_eq!(locate_nemotron_dir(&root), Some(good));
    }

    #[test]
    fn sweep_removes_stale_staging_only() {
        let tmp = TempDir::new().unwrap();
        let store = CacheDirStore {
            root: tmp.path().to_path_buf(),
        };
        // Stale: a part file from a previous attempt + a tmp dir.
        write(&tmp.path().join("tiny.en.1.gguf.part"), b"x");
        fs::create_dir_all(tmp.path().join("tiny.en.1.gguf.tmp")).unwrap();
        // Real: an installed gguf and an unrelated file.
        write(&tmp.path().join("tiny.en.gguf"), b"real");
        write(&tmp.path().join("README"), b"keep me");

        store.sweep_staging().unwrap();
        assert!(!tmp.path().join("tiny.en.1.gguf.part").exists());
        assert!(!tmp.path().join("tiny.en.1.gguf.tmp").exists());
        assert!(tmp.path().join("tiny.en.gguf").exists());
        assert!(tmp.path().join("README").exists());
    }

    #[test]
    fn every_catalog_format_has_a_handler() {
        // Adding a ModelFormat without extending `handler_for` would
        // be a runtime panic. This test makes that a compile-time
        // obligation: the match below forces the compiler to keep
        // `handler_for` exhaustive.
        for entry in CATALOG {
            let _ = handler_for(entry.format);
        }
    }
}
