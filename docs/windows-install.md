# Voice Bird CLI on Windows

> **Windows is cloud-only since 0.4.0.** Local Whisper inference (and the
> Nemotron local engine) are not built on Windows — transcription streams to
> VoiceBird Web, which requires a Voice Bird API key. In exchange, installing
> no longer needs CMake, LLVM/libclang, or a whisper.cpp compile: the standard
> Rust MSVC toolchain is enough.
>
> There is still no prebuilt Windows binary — `cargo`, `npm`, and `pip` all
> compile the (pure-Rust) binary from source. The build takes a couple of
> minutes, not the 5–10 of the old whisper.cpp builds.

---

## Install

1. Install **Rust** via [rustup](https://rustup.rs/) with the default **MSVC**
   toolchain (`x86_64-pc-windows-msvc`). Rustup will prompt you to install the
   **Visual Studio C++ Build Tools** if they're missing — that provides
   `link.exe` and the Windows SDK, and it is the only C/C++ tooling you need.
2. Build from the **"x64 Native Tools Command Prompt for VS 2022"** (see the
   linker note below), then:
   ```cmd
   cargo install voice-bird-cli --locked
   :: or:  npm install -g voice-bird-cli
   :: or:  pip install voice-bird-cli  (then run `voice-bird-cli` once to build)
   ```
3. Run `voice-bird-cli`. On first launch it opens the **API-key prompt** —
   paste your Voice Bird API key (from your VoiceBird Web account) and press
   Enter. Press `c` any time to change the key.

## What's different on Windows

- **Cloud-only.** Every recording streams to VoiceBird Web
  (`wss://voicebird.app/api/audio/stream`); recordings live in your VoiceBird
  Web account, not in a local sessions folder.
- **No local models** — there is no model picker (`m`), no model downloads,
  and no export (`e`) / session-path (`p`) keys. Language selection (`l`) is
  always available, since cloud transcription supports multiple languages.
- `c` opens the API-key dialog (it does not toggle local mode — there is none).
- `voice-bird-cli --recover <session-dir>` still works, but only matters for
  session folders created on macOS/Linux.

## Known issue — wrong `link.exe` on `PATH`

**Symptom** — the build dies almost immediately, on trivial crates, with:

```
error: linking with `link.exe` failed: exit code: 1
  = note: link: extra operand '...rcgu.o'
          Try 'link --help' for more information.
```

**Cause** — that is GNU coreutils `link` (a thin `ln` wrapper), not MSVC's
linker. It ships with **Git for Windows** (`C:\Program Files\Git\usr\bin\`)
and MSYS2/Cygwin; if that directory precedes the MSVC `bin` directory on your
`PATH`, `rustc` invokes the wrong tool. This affects *any* Rust MSVC build,
not just Voice Bird.

**Fix** — build from the **"x64 Native Tools Command Prompt for VS 2022"**
(Start menu), which puts the MSVC linker first. Confirm with:

```cmd
where link
:: want: ...\VC\Tools\MSVC\<ver>\bin\Hostx64\x64\link.exe
:: NOT:  ...\Git\usr\bin\link.exe
```

## Verifying the install

The binary has no headless `--version` (it opens an audio device on startup).

- **Binary:** `%USERPROFILE%\.cargo\bin\voice-bird-cli.exe`
- **Version marker** (written by the npm/pip wrappers):
  ```cmd
  type "%USERPROFILE%\.cargo\bin\.voice-bird-cli.version"
  ```
  A direct `cargo install` does not write the marker; use
  `cargo install --list | findstr voice-bird` instead.
- **Logs:** `%USERPROFILE%\.voice-bird\logs\voice_bird_*.log`
- **Config (API key, plaintext):** `%APPDATA%\voice-bird\config.toml`

## History — why Windows went cloud-only

Versions ≤ 0.3.x built local Whisper inference on every platform, which on
Windows required Visual Studio C++ Build Tools **plus** CMake **plus** an
LLVM/libclang that bindgen could use — and broke three different ways in
practice: the coreutils `link.exe` collision above, an Apple-only `metal`
feature leaking into non-Apple builds (fixed in 0.3.4), and a bindgen
0.69 × libclang 19+ incompatibility that emitted broken whisper bindings
(workaround was pinning a side-by-side LLVM 18 via `LIBCLANG_PATH`). Rather
than ask users to assemble that toolchain, 0.4.0 removed local inference on
Windows; macOS and Linux keep full local-first support.
