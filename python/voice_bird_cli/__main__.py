from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

try:
    from voice_bird_cli import __version__
except Exception:  # pragma: no cover - package metadata should always be present
    __version__ = None


def _cargo_bin() -> Path:
    name = "voice-bird-cli.exe" if os.name == "nt" else "voice-bird-cli"
    return Path.home() / ".cargo" / "bin" / name


def _marker() -> Path:
    return Path.home() / ".cargo" / "bin" / ".voice-bird-cli.version"


def _installed_version() -> str | None:
    # The native binary has no headless `--version` (it opens an audio device),
    # so we read the version we recorded when we last installed it.
    try:
        return _marker().read_text().strip()
    except OSError:
        return None


def _needs_install(exe: Path) -> bool:
    if not exe.exists():
        return True
    if __version__ is None:
        return False  # binary present, can't determine target — assume it's fine
    return _installed_version() != __version__


def _install() -> int:
    # Pin to this package's exact version and force, so an existing older binary
    # is upgraded rather than left in place when the pip package is updated.
    cmd = ["cargo", "install", "voice-bird-cli", "--locked", "--force"]
    if __version__:
        cmd[3:3] = ["--version", __version__]
    try:
        subprocess.run(cmd, check=True)
    except FileNotFoundError:
        print(
            "voice-bird-cli PyPI package requires Rust Cargo. "
            "Install Rust from https://rustup.rs/ and run this command again.",
            file=sys.stderr,
        )
        return 1
    except subprocess.CalledProcessError as exc:
        return exc.returncode
    if __version__:
        try:
            _marker().write_text(f"{__version__}\n")
        except OSError:
            pass
    return 0


def main() -> int:
    exe = _cargo_bin()
    if _needs_install(exe):
        rc = _install()
        # Only abort if we have no usable binary; otherwise run the older one.
        if rc != 0 and not exe.exists():
            return rc
    return subprocess.run([str(exe), *sys.argv[1:]]).returncode


if __name__ == "__main__":
    raise SystemExit(main())
