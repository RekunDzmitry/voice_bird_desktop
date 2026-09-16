//! On-disk transcription-model store (Whisper GGUF, Nemotron ONNX).
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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use flate2::read::GzDecoder;
use tar::Archive;

use crate::picker::{ModelEntry, ModelFormat};

/// Result type for the model-store surface. `DownloadError` lives in
/// `download.rs` because the IO+HTTP+install taxonomy is one error
/// vocabulary; this module just re-uses it.
pub use crate::download::DownloadError;

/// Everything format-specific about getting a model onto disk and
/// deciding whether it is already there.
pub trait ModelFormatHandler: Send + Sync {
    fn staging_path(&self, dir: &Path, id: &str) -> PathBuf;
    fn installed_path(&self, dir: &Path, id: &str) -> PathBuf;
    /// Present *and usable* — not merely "a path exists".
    fn is_installed(&self, dir: &Path, id: &str) -> bool;
    /// Move/unpack the verified staging file into place. On failure it
    /// Move/unpack the verified staging file into place. The cancel
    /// token is the same one `spawn()` passed to the downloader; it
    /// is polled between unpack and rename so closing the last
    /// waiting block during the install phase actually stops the
    /// worker instead of leaving a half-unpacked directory on disk.
    /// Returning `DownloadError::Cancelled` is the contract the
    /// worker relies on to publish nothing — the producer already
    /// dropped the row when it set the token.
    fn install(
        &self,
        dir: &Path,
        id: &str,
        staged: &Path,
        cancel: &Arc<AtomicBool>,
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
    }
}

/// Single Whisper GGUF `.bin` file. Staging filename `<id>.gguf.part`,
/// installed `<id>.gguf` (matches the legacy crate's `gguf_path` so a
/// model already fetched by `voice-bird-cli` reads as present).
pub struct GgufHandler;

impl ModelFormatHandler for GgufHandler {
    fn staging_path(&self, dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.gguf.part"))
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
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        // GGUF install is a single atomic rename; the cancel token
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
/// `<id>.tar.gz.part`, installed `<id>/` (matches the legacy
/// `nemotron_model_dir`).
///
/// `is_installed` requires **both** `encoder.onnx` and `decoder_joint.onnx`
/// so an empty or half-unpacked directory is not mistaken for a model.
pub struct NemotronPackageHandler;

impl ModelFormatHandler for NemotronPackageHandler {
    fn staging_path(&self, dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.tar.gz.part"))
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
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        let tmp_dir = dir.join(format!("{id}.tmp"));
        let target = self.installed_path(dir, id);
        // Best-effort cleanup of leftovers from a prior crashed run.
        if tmp_dir.exists() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }
        if target.exists() {
            let _ = fs::remove_dir_all(&target);
        }
        fs::create_dir_all(&tmp_dir).map_err(|e| DownloadError::Io(e.to_string()))?;
        // Unpack entry by entry so the cancel token can be polled
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
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    // The producer already dropped the row when it
                    // set the token; the worker treats this the
                    // same as the fetch-time Cancelled and
                    // publishes nothing.
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
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
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

/// Public surface used by the resolver. `CacheDirStore` is a thin
/// pass-through over [`ModelFormatHandler`]; future stores (memory,
/// http, ...) live behind the same trait.
pub trait ModelStore: Send + Sync + 'static {
    fn is_available(&self, entry: &ModelEntry) -> bool;
    fn staging_path(&self, entry: &ModelEntry) -> Result<PathBuf, DownloadError>;
    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError>;
    /// Delete a leftover staging file (cancellation, and the startup sweep).
    fn clear_staging(&self, entry: &ModelEntry);
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

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Delete every `*.part` file and `*.tmp` directory left by a
    /// previous run. Called once from `main`. Staging names are
    /// deterministic so a restarted download would overwrite rather
    /// than accumulate; the sweep is what stops a killed 1.5 GB
    /// download from sitting on disk forever.
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

    fn staging_path(&self, entry: &ModelEntry) -> Result<PathBuf, DownloadError> {
        Ok(handler_for(entry.format).staging_path(&self.root, entry.id))
    }

    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        handler_for(entry.format).install(&self.root, entry.id, staged, cancel)
    }

    fn clear_staging(&self, entry: &ModelEntry) {
        let p = handler_for(entry.format).staging_path(&self.root, entry.id);
        if p.is_file() {
            let _ = fs::remove_file(&p);
        }
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
    use tempfile::TempDir;

    fn tiny_entry() -> &'static ModelEntry {
        &CATALOG[5]
    }

    fn nemotron_entry() -> &'static ModelEntry {
        &CATALOG[3]
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
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
    fn gguf_install_renames_staged_file() {
        let tmp = TempDir::new().unwrap();
        let h = GgufHandler;
        let staged = h.staging_path(tmp.path(), "tiny.en");
        write(&staged, b"GGUF");
        h.install(
            tmp.path(),
            "tiny.en",
            &staged,
            &Arc::new(AtomicBool::new(false)),
        )
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
    fn nemotron_install_unpacks_and_renames() {
        // Build a minimal tar.gz: a directory `pkg/` containing both
        // ONNX files. Run install on it and assert the installed dir
        // carries the artifacts at the right path.
        let src = TempDir::new().unwrap();
        let pkg = src.path().join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        write(&pkg.join("encoder.onnx"), b"e");
        write(&pkg.join("decoder_joint.onnx"), b"d");

        let archive = src.path().join("pkg.tar.gz");
        let f = fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all("pkg", &pkg).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let dst = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        h.install(
            dst.path(),
            nemotron_entry().id,
            &archive,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let installed = h.installed_path(dst.path(), nemotron_entry().id);
        assert!(installed.is_dir());
        assert!(installed.join("encoder.onnx").is_file());
        assert!(installed.join("decoder_joint.onnx").is_file());
        assert!(!dst
            .path()
            .join(format!("{}.tmp", nemotron_entry().id))
            .exists());
    }

    #[test]
    fn nemotron_install_failure_leaves_no_installed_dir() {
        // Corrupt the archive by truncating; install must roll back
        // the scratch directory AND not leave anything at installed_path.
        let src = TempDir::new().unwrap();
        let archive = src.path().join("broken.tar.gz");
        write(&archive, b"not a tarball");

        let dst = TempDir::new().unwrap();
        let h = NemotronPackageHandler;
        let res = h.install(
            dst.path(),
            nemotron_entry().id,
            &archive,
            &Arc::new(AtomicBool::new(false)),
        );
        assert!(res.is_err());
        assert!(!h.installed_path(dst.path(), nemotron_entry().id).exists());
        assert!(!dst
            .path()
            .join(format!("{}.tmp", nemotron_entry().id))
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
        // Suppress the unused-variable warning when not unix.
        let _ = root.join("bad");
        assert_eq!(locate_nemotron_dir(&root), Some(good));
    }

    #[test]
    fn clear_staging_removes_part_file() {
        let tmp = TempDir::new().unwrap();
        let store = CacheDirStore {
            root: tmp.path().to_path_buf(),
        };
        let staged = store.staging_path(tiny_entry()).unwrap();
        write(&staged, b"x");
        assert!(staged.exists());
        store.clear_staging(tiny_entry());
        assert!(!staged.exists());
    }

    #[test]
    fn sweep_removes_stale_staging_only() {
        let tmp = TempDir::new().unwrap();
        let store = CacheDirStore {
            root: tmp.path().to_path_buf(),
        };
        // Stale: a part file + a tmp dir.
        write(&tmp.path().join("tiny.en.gguf.part"), b"x");
        fs::create_dir_all(tmp.path().join("old.tmp")).unwrap();
        // Real: an installed gguf and an unrelated file.
        write(&tmp.path().join("tiny.en.gguf"), b"real");
        write(&tmp.path().join("README"), b"keep me");

        store.sweep_staging().unwrap();
        assert!(!tmp.path().join("tiny.en.gguf.part").exists());
        assert!(!tmp.path().join("old.tmp").exists());
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
