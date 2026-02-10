from __future__ import annotations

import os
import sys
import sysconfig


def find_voice_bird_cli() -> str:
    """Return the voice-bird-cli binary path."""
    exe = "voice-bird-cli" + (sysconfig.get_config_var("EXE") or "")

    scripts_path = os.path.join(sysconfig.get_path("scripts"), exe)
    if os.path.isfile(scripts_path):
        return scripts_path

    if sys.version_info >= (3, 10):
        user_scheme = sysconfig.get_preferred_scheme("user")
    elif os.name == "nt":
        user_scheme = "nt_user"
    elif sys.platform == "darwin" and sys._framework:
        user_scheme = "osx_framework_user"
    else:
        user_scheme = "posix_user"

    user_path = os.path.join(sysconfig.get_path("scripts", scheme=user_scheme), exe)
    if os.path.isfile(user_path):
        return user_path

    pkg_root = os.path.dirname(os.path.dirname(__file__))
    target_path = os.path.join(pkg_root, "bin", exe)
    if os.path.isfile(target_path):
        return target_path

    raise FileNotFoundError(
        f"Could not find {exe}. Searched:\n"
        f"  {scripts_path}\n"
        f"  {user_path}\n"
        f"  {target_path}"
    )


if __name__ == "__main__":
    bin_path = find_voice_bird_cli()
    if sys.platform == "win32":
        import subprocess

        completed_process = subprocess.run([bin_path, *sys.argv[1:]])
        sys.exit(completed_process.returncode)
    else:
        os.execvp(bin_path, [bin_path, *sys.argv[1:]])
