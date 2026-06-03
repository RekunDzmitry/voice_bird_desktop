from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def _cargo_bin() -> Path:
    name = "voice-bird-cli.exe" if os.name == "nt" else "voice-bird-cli"
    return Path.home() / ".cargo" / "bin" / name


def main() -> int:
    exe = _cargo_bin()
    if not exe.exists():
        try:
            subprocess.run(
                ["cargo", "install", "voice-bird-cli", "--locked"],
                check=True,
            )
        except FileNotFoundError:
            print(
                "voice-bird-cli PyPI package requires Rust Cargo. "
                "Install Rust from https://rustup.rs/ and run this command again.",
                file=sys.stderr,
            )
            return 1
        except subprocess.CalledProcessError as exc:
            return exc.returncode

    return subprocess.run([str(exe), *sys.argv[1:]]).returncode


if __name__ == "__main__":
    raise SystemExit(main())
