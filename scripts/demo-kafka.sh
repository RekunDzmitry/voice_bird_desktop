#!/bin/bash
# Manual pre-release Kafka e2e demo (issue #34).
#
# Given a reachable broker on localhost:9092 (or a Docker daemon so it
# can start one), this script exercises the real TUI binary end to end:
#
#   1. Starts a single-node KRaft broker via Docker if localhost:9092
#      is not already listening.
#   2. Launches the TUI with an isolated $HOME and drives the
#      Add-Agent funnel key by key through a PTY: name, endpoint,
#      topic, acks, security, then the VERIFY step (a real
#      produce/consume round trip through the TUI's own code path)
#      and Save.
#   3. Asserts the funnel-saved target landed in config.toml, that
#      the verify probe record landed on the topic, and that the
#      dispatcher logged no WARN lines.
#   4. Runs the env-gated `tests/agent_kafka_e2e.rs` suite against the
#      same broker for segment push -> consume coverage. (The
#      recorder's consumer-dispatch arms are unit-tested — see
#      dispatch_segment_to_agent in src/app.rs — so the demo does not
#      need live audio; full audio automation lives in e2e_human/.)
#   5. Cleans up the broker if it started one, and prints a one-line
#      summary suitable for a release thread:
#        kafka demo: ok (verify 1234ms, 12 segments landed, 0 warnings)
#        kafka demo: FAIL (<step>: <reason>)
#
# Manual by design: it can spin up a broker, so it runs before each
# release (see scripts/release.sh --with-kafka) rather than in CI. CI
# regression coverage stays with the env-gated e2e test, which skips
# cleanly when no broker is configured.
set -uo pipefail

cd "$(dirname "$0")/.."

BROKER="localhost:9092"
CONTAINER="voice-bird-kafka-demo"
KAFKA_IMAGE="apache/kafka:3.7.2"
TOPIC="voice-bird-demo-$(date +%s)"
STARTED_BROKER=0
DEMO_HOME="$(mktemp -d /tmp/voice-bird-kafka-demo.XXXXXX)"
SUMMARY_FAIL_STEP=""

fail() {
  SUMMARY_FAIL_STEP="$1"
  shift
  echo "kafka demo: FAIL (${SUMMARY_FAIL_STEP}: $*)" >&2
  # Keep $DEMO_HOME for debugging on failure — it holds driver.log,
  # the state-snapshot JSONL, and the TUI logs.
  echo "kafka demo: artifacts kept in $DEMO_HOME" >&2
  if [ "$STARTED_BROKER" = "1" ]; then
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  fi
  exit 1
}

cleanup() {
  if [ "$STARTED_BROKER" = "1" ]; then
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  fi
  rm -rf "$DEMO_HOME"
}

broker_listening() {
  (exec 3<>"/dev/tcp/localhost/9092") 2>/dev/null && { exec 3>&-; return 0; }
  return 1
}

# ── 1. Broker ─────────────────────────────────────────────────────────
if broker_listening; then
  echo "broker: reusing existing listener on $BROKER"
else
  command -v docker >/dev/null || fail broker "nothing on $BROKER and docker not installed"
  echo "broker: starting $KAFKA_IMAGE in Docker..."
  docker run -d --rm --name "$CONTAINER" -p 9092:9092 "$KAFKA_IMAGE" >/dev/null \
    || fail broker "docker run $KAFKA_IMAGE failed"
  STARTED_BROKER=1
  for _ in $(seq 1 60); do
    broker_listening && break
    sleep 1
  done
  broker_listening || fail broker "broker did not come up on $BROKER within 60s"
  # Port accepting connections != broker ready to serve; give KRaft a
  # moment to finish its startup election.
  sleep 3
fi

# ── 2. Build + drive the TUI funnel ──────────────────────────────────
echo "build: cargo build --bin voice-bird-cli"
cargo build --bin voice-bird-cli >/dev/null 2>&1 || fail build "cargo build failed"

echo "tui: driving the Add-Agent funnel (isolated HOME=$DEMO_HOME)"
DEMO_HOME="$DEMO_HOME" TOPIC="$TOPIC" BROKER="$BROKER" python3 scripts/demo_kafka_driver.py
DRIVER_RC=$?
[ "$DRIVER_RC" = "0" ] || fail tui "funnel driver exited $DRIVER_RC (see $DEMO_HOME/driver.log)"

VERIFY_MS="$(cat "$DEMO_HOME/verify_ms" 2>/dev/null || echo "?")"

# ── 3. Assertions on the artifacts ───────────────────────────────────
CONFIG_FILE="$(find "$DEMO_HOME" -name config.toml -path '*voice-bird*' | head -1)"
[ -n "$CONFIG_FILE" ] || fail config "no config.toml written under $DEMO_HOME"
grep -q "topic = \"$TOPIC\"" "$CONFIG_FILE" \
  || fail config "funnel-saved target (topic $TOPIC) not found in $CONFIG_FILE"

WARNINGS=$(cat "$DEMO_HOME"/.voice-bird/logs/*.log 2>/dev/null | grep -c " WARN ")
[ "$WARNINGS" = "0" ] || {
  grep " WARN " "$DEMO_HOME"/.voice-bird/logs/*.log >&2
  fail warnings "$WARNINGS WARN line(s) in the TUI log"
}

if [ "$STARTED_BROKER" = "1" ]; then
  # The funnel's verify step produced a probe record; confirm it is
  # actually on the topic via the broker's own console consumer.
  LANDED=$(docker exec "$CONTAINER" /opt/kafka/bin/kafka-console-consumer.sh \
    --bootstrap-server localhost:9092 --topic "$TOPIC" \
    --from-beginning --timeout-ms 10000 2>/dev/null | grep -c "voice_bird_agent_verify")
  [ "$LANDED" -ge 1 ] || fail consume "verify probe not found on topic $TOPIC"
else
  echo "consume: skipping console-consumer check (broker not managed by this script)"
fi

# ── 4. Segment push -> consume coverage over the same broker ─────────
echo "e2e: TEST_KAFKA_BROKER=$BROKER cargo test --test agent_kafka_e2e"
TEST_KAFKA_BROKER="$BROKER" TEST_KAFKA_TOPIC="${TOPIC}-e2e" \
  cargo test --test agent_kafka_e2e >"$DEMO_HOME/e2e.log" 2>&1 \
  || { tail -20 "$DEMO_HOME/e2e.log" >&2; fail e2e "agent_kafka_e2e failed against $BROKER"; }
SEGMENTS=$(grep -Eo "test result: ok\. [0-9]+ passed" "$DEMO_HOME/e2e.log" | grep -Eo "[0-9]+" | head -1)

# ── 5. Summary ───────────────────────────────────────────────────────
cleanup
echo "kafka demo: ok (verify ${VERIFY_MS}ms, ${SEGMENTS:-?} e2e tests landed segments, 0 warnings)"
