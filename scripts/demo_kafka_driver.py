#!/usr/bin/env python3
"""PTY driver for scripts/demo-kafka.sh (issue #34).

Launches the TUI with an isolated $HOME, walks the Add-Agent funnel
key by key (Targets pane -> a -> name/endpoint/topic/acks/security ->
Verify -> Save), and exits 0 once the funnel banner confirms the
target was saved. State is observed through the TUI's own
--debug-state-snapshot JSONL (written on every key and once per
second), so every step waits for the app to actually reach the
expected state instead of sleeping blind.

Env: DEMO_HOME (isolated home dir), BROKER (host:port), TOPIC.
Writes DEMO_HOME/verify_ms with the verify round-trip time on success.
Stdlib only.
"""

import json
import os
import pty
import signal
import subprocess
import sys
import threading
import time

DEMO_HOME = os.environ["DEMO_HOME"]
BROKER = os.environ["BROKER"]
TOPIC = os.environ["TOPIC"]
SNAPSHOT = os.path.join(DEMO_HOME, "state.jsonl")
LOG = open(os.path.join(DEMO_HOME, "driver.log"), "w")

RIGHT = "\x1b[C"
ENTER = "\r"


def log(msg):
    LOG.write(f"[{time.strftime('%H:%M:%S')}] {msg}\n")
    LOG.flush()


def last_snapshot():
    try:
        with open(SNAPSHOT) as f:
            lines = f.read().strip().splitlines()
        return json.loads(lines[-1]) if lines else {}
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def wait_for(predicate, what, timeout=30.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        snap = last_snapshot()
        if predicate(snap):
            log(f"reached: {what}")
            return snap
        time.sleep(0.2)
    snap = last_snapshot()
    log(f"TIMEOUT waiting for {what}; last snapshot: {json.dumps(snap)}")
    print(f"driver: timeout waiting for {what}", file=sys.stderr)
    raise SystemExit(3)


def main():
    env = dict(os.environ)
    env["HOME"] = DEMO_HOME
    # Linux config-path resolution goes through XDG.
    env["XDG_CONFIG_HOME"] = os.path.join(DEMO_HOME, ".config")

    master, slave = pty.openpty()
    proc = subprocess.Popen(
        ["target/debug/voice-bird-cli", "--debug-state-snapshot", SNAPSHOT],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        preexec_fn=os.setsid,
    )
    os.close(slave)

    # Drain the TUI's rendered frames continuously. Without a reader
    # the PTY buffer fills after a few redraws and the app blocks on
    # its stdout write — frozen event loop, no more snapshots.
    def drain():
        try:
            while True:
                if not os.read(master, 65536):
                    return
        except OSError:
            return

    threading.Thread(target=drain, daemon=True).start()

    def send(keys, label):
        log(f"send: {label}")
        os.write(master, keys.encode())
        time.sleep(0.5)
        snap = last_snapshot()
        log(
            f"  after: mode={snap.get('mode')} focus={snap.get('picker_focus')} "
            f"funnel_step={snap.get('funnel_step')} banner={snap.get('banner')}"
        )

    try:
        wait_for(lambda s: s.get("mode") == "Normal", "startup")

        send(RIGHT + RIGHT, "focus Targets pane")
        wait_for(lambda s: s.get("picker_focus") == "Targets", "Targets focus")

        send("a", "open Add-Agent funnel")
        wait_for(lambda s: s.get("funnel_step") == "PickConnectionKind", "funnel open")

        send(ENTER, "connection kind: Kafka")
        send("demo", "name")
        send(ENTER, "advance to endpoint")
        send(BROKER, "endpoint")
        send(ENTER, "advance to topic")
        send(TOPIC, "topic")
        send(ENTER, "advance to acks")
        send(ENTER, "acks: All (default)")
        send(ENTER, "security: plaintext (default)")
        wait_for(lambda s: s.get("funnel_step") == "Verify", "Verify step")

        send(ENTER, "run verify probe")
        snap = wait_for(
            lambda s: str(s.get("funnel_verify", "")).startswith(("Ok:", "Err:")),
            "verify result",
            timeout=45.0,
        )
        verify = snap["funnel_verify"]
        if verify.startswith("Err:"):
            log(f"verify failed: {verify}")
            print(f"driver: verify failed: {verify}", file=sys.stderr)
            raise SystemExit(4)
        verify_ms = verify.removeprefix("Ok:").removesuffix("ms")
        with open(os.path.join(DEMO_HOME, "verify_ms"), "w") as f:
            f.write(verify_ms)

        send(ENTER, "advance to Save")
        wait_for(lambda s: s.get("funnel_step") == "Save", "Save step")
        send(ENTER, "save target")
        wait_for(
            lambda s: s.get("mode") == "Normal"
            and "Saved Agent target" in (s.get("banner") or ""),
            "saved banner",
        )

        send("q", "quit")
        proc.wait(timeout=15)
        log("clean exit")
        return 0
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
        os.close(master)


if __name__ == "__main__":
    sys.exit(main())
