//! End-to-end test for the user-configured Kafka Agent target.
//!
//! The test exercises the full funnel → verify → push path:
//!
//! 1. Build a `KafkaTarget` from a config the user would have
//!    produced via the funnel (endpoint + topic + acks).
//! 2. Drive the verify step (probe → consume round-trip).
//! 3. Push a synthetic `Segment` through the trait.
//! 4. Subscribe a `StreamConsumer` to the same topic from
//!    offset 0 and assert the JSON line we read back matches
//!    the segment we pushed.
//!
//! **Skip condition:** the test reads `TEST_KAFKA_BROKER` and
//! `TEST_KAFKA_TOPIC` from the environment. If either is unset
//! it prints a skip notice and returns success — CI stays green
//! without a broker. To run it locally, point at any reachable
//! broker:
//!
//! ```bash
//! docker run -d --rm -p 9092:9092 \
//!   -e KAFKA_NODE_ID=0 \
//!   -e KAFKA_PROCESS_ROLES=broker,controller \
//!   -e KAFKA_CONTROLLER_QUORUM_VOTERS=0@localhost:9093 \
//!   -e KAFKA_LISTENERS=PLAINTEXT://0.0.0.0:9092,CONTROLLER://0.0.0.0:9093 \
//!   -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://localhost:9092 \
//!   -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=PLAINTEXT:PLAINTEXT,CONTROLLER:PLAINTEXT \
//!   -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
//!   -e KAFKA_INTER_BROKER_LISTENER_NAME=PLAINTEXT \
//!   apache/kafka:3.7.0
//!
//! TEST_KAFKA_BROKER=localhost:9092 TEST_KAFKA_TOPIC=voice-bird-e2e \
//!   cargo test --test agent_kafka_e2e -- --nocapture
//! ```

use std::future::Future;
use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;

use voice_bird_cli::agent::kafka::KafkaTarget;
use voice_bird_cli::agent::{AgentSessionId, AgentTarget};
use voice_bird_cli::config::KafkaAgentConnection;
use voice_bird_cli::transcription::Segment;

/// Skip the test if the env vars aren't set. The skip is
/// loud (printed) so the operator running locally knows
/// the test exists and how to enable it.
fn broker_or_skip() -> Option<(String, String)> {
    let broker = std::env::var("TEST_KAFKA_BROKER").ok()?;
    let topic = std::env::var("TEST_KAFKA_TOPIC").ok()?;
    if broker.is_empty() || topic.is_empty() {
        eprintln!("agent_kafka_e2e: TEST_KAFKA_BROKER or TEST_KAFKA_TOPIC is empty; skipping");
        return None;
    }
    Some((broker, topic))
}

/// Drive a future synchronously. Cargo test's plain
/// `#[test]` runner is sync, so we can't `block_on`
/// directly on the caller's thread; the
/// `FutureProducer`'s background tasks need a real
/// reactor, so the test spawns a thread + own
/// current-thread runtime to drive the future.
fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build one-shot tokio runtime");
        rt.block_on(fut)
    })
    .join()
    .expect("join test thread")
}

/// Build a fresh consumer for the read-back half of the
/// test. We don't reuse the producer's `KafkaTarget`
/// because the target only exposes the produce side.
fn build_consumer(broker: &str) -> StreamConsumer {
    let mut cfg = ClientConfig::new();
    cfg.set("bootstrap.servers", broker)
        .set(
            "group.id",
            format!("voice-bird-e2e-{}", uuid::Uuid::new_v4()),
        )
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .set("session.timeout.ms", "6000");
    cfg.create().expect("create consumer")
}

#[test]
fn funnel_to_kafka_round_trip() {
    let Some((broker, topic)) = broker_or_skip() else {
        return;
    };
    eprintln!("agent_kafka_e2e: running against broker={broker} topic={topic}");

    // Run on a fresh runtime so the test is self-contained.
    let result: anyhow::Result<()> = block_on(async move {
        // 1. The funnel-configured connection. In the TUI this
        //    is what the user would see on the Save step.
        let conn = KafkaAgentConnection {
            endpoint: broker.clone(),
            topic: topic.clone(),
            client_id: Some("voice-bird-e2e".into()),
            acks: Default::default(),
        };
        let target = KafkaTarget::new(AgentSessionId("e2e-session".into()), conn.clone());

        // 2. Verify the connection. This is the same call
        //    the funnel's Verify step makes. We do it once
        //    up front to make sure the broker is reachable
        //    before we start consuming.
        let verify_elapsed = target
            .verify()
            .await
            .map_err(|e| anyhow::anyhow!("verify failed: {e}"))?;
        eprintln!("verify: OK in {verify_elapsed:?}");

        // 3. Build the segment we'd hand the recorder's
        //    consumer task in production. The same
        //    `AgentTarget::push_segment` call shape.
        let seg = Segment {
            t_start: Duration::from_millis(0),
            t_end: Duration::from_millis(750),
            text: "hello kafka e2e".into(),
            tokens: Vec::new(),
        };
        target
            .push_segment(&seg)
            .map_err(|e| anyhow::anyhow!("push_segment failed: {e}"))?;

        // 4. Build a read-back consumer and `assign()` it from
        //    offset 0 on every partition. We use `assign` rather
        //    than `subscribe` so the test doesn't depend on a
        //    consumer-group join — `subscribe` round-trips the
        //    coordinator and can chew the wall-clock budget on a
        //    cold broker before the first poll returns. `assign`
        //    is a local command and starts polling on the next
        //    `recv()` call. We use a 30 s wall-clock ceiling to
        //    absorb broker bring-up on a cold test cluster.
        let consumer = build_consumer(&broker);
        let md = consumer
            .fetch_metadata(Some(&topic), Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("read-back metadata fetch failed: {e}"))?;
        let partitions = md
            .topics()
            .iter()
            .find(|t| t.name() == topic)
            .ok_or_else(|| anyhow::anyhow!("read-back: topic '{topic}' not in metadata"))?
            .partitions();
        let mut tpl = rdkafka::TopicPartitionList::new();
        for p in partitions {
            tpl.add_partition_offset(
                &topic,
                p.id(),
                rdkafka::topic_partition_list::Offset::Offset(0),
            )?;
        }
        consumer.assign(&tpl)?;

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("timed out after 30 s without seeing the segment on '{topic}'");
            }
            match tokio::time::timeout(Duration::from_millis(500), consumer.recv()).await {
                Ok(Ok(msg)) => {
                    let Some(payload) = msg.payload() else {
                        continue;
                    };
                    let value: serde_json::Value = match serde_json::from_slice(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // The verify probe is also a JSON line on
                    // this topic — skip past it and only stop
                    // when we see the segment's text.
                    let text = value.get("text").and_then(|v| v.as_str());
                    if text == Some(seg.text.as_str()) {
                        // Final check: the segment's t_start
                        // round-trips as the key. The Kafka
                        // target uses segment.t_start as the
                        // partition key, so this is the
                        // canonical place to verify the wire
                        // format.
                        let key = msg.key().and_then(|k| std::str::from_utf8(k).ok());
                        if key != Some("0") {
                            anyhow::bail!(
                                "expected key '0' (segment.t_start as ms), got {:?}",
                                key
                            );
                        }
                        eprintln!("read back: text={text:?} key={key:?}");
                        return Ok(());
                    }
                }
                Ok(Err(rdkafka::error::KafkaError::PartitionEOF(_))) => continue,
                Ok(Err(e)) => anyhow::bail!("recv error: {e}"),
                Err(_) => continue, // wall-clock timeout, keep polling
            }
        }
    });

    result.expect("Kafka round-trip test failed");
    eprintln!("agent_kafka_e2e: passed");
}

/// A second, lighter test that exercises just the verify
/// path (no segment push, no consumer). Catches "the
/// broker is reachable but produce is blocked" failures
/// the main test would also catch — included separately
/// so an operator can run `cargo test verify_only` to
/// smoke-test their broker config.
#[test]
fn verify_only() {
    let Some((broker, topic)) = broker_or_skip() else {
        return;
    };
    let conn = KafkaAgentConnection {
        endpoint: broker,
        topic,
        client_id: None,
        acks: Default::default(),
    };
    let target = KafkaTarget::new(AgentSessionId("verify-only".into()), conn);
    // Drive verify on its own thread+runtime. We can't
    // `block_on` from inside the test runtime (cargo test
    // itself is async), so spawn a fresh thread that
    // builds a one-shot runtime for the verify call.
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<Duration>>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build one-shot tokio runtime");
        let _ = tx.send(rt.block_on(target.verify()));
    });
    let elapsed = rx
        .recv()
        .expect("join verify thread")
        .expect("verify should succeed against a reachable broker");
    eprintln!("verify_only: OK in {elapsed:?}");
}

/// R6 (PR #31 round-3 review): verifying a BRAND-NEW topic must
/// succeed on the first attempt against a broker with topic
/// auto-creation enabled (the default single-node dev setup in
/// this file's module docs). The assign()-at-watermarks rework
/// fetches metadata *before* producing anything, and the first
/// metadata request for a nonexistent topic returns no usable
/// partitions (LEADER_NOT_AVAILABLE) — so the very first verify
/// of a freshly typed topic name fails where the old
/// produce-first flow auto-created the topic and passed.
///
/// Gated like the other tests here: set TEST_KAFKA_BROKER (and
/// TEST_KAFKA_TOPIC, unused here but required by the shared
/// skip helper) to run.
#[test]
fn verify_fresh_topic_first_attempt() {
    let Some((broker, _existing_topic)) = broker_or_skip() else {
        return;
    };
    let fresh_topic = format!("voice-bird-e2e-fresh-{}", uuid::Uuid::new_v4());
    let conn = KafkaAgentConnection {
        endpoint: broker,
        topic: fresh_topic.clone(),
        client_id: None,
        acks: Default::default(),
    };
    let target = KafkaTarget::new(AgentSessionId("fresh-verify".into()), conn);
    let result = block_on(async move { target.verify().await });
    match result {
        Ok(elapsed) => eprintln!("fresh-topic verify: OK in {elapsed:?}"),
        Err(e) => panic!("first verify of brand-new topic '{fresh_topic}' must succeed (R6): {e}"),
    }
}

/// Regression test for the busy-topic verify race (PR #31, reviewer
/// note on the produce-first rework): with the watermark-based
/// assign, any record landing on the probe's partition between the
/// produce-ack and the watermark fetch pushed `high - 1` past the
/// probe, and verify timed out against a perfectly healthy broker.
/// The delivery-report assign positions the consumer at the probe's
/// exact `(partition, offset)`, so concurrent traffic can't hide it.
///
/// The test floods a dedicated topic from a background thread while
/// running verify twice — once while the topic is being auto-created
/// under load, once against the established busy topic. Both must
/// succeed. Gated on TEST_KAFKA_BROKER like the rest of this file.
#[test]
fn verify_busy_topic() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let Some((broker, _shared_topic)) = broker_or_skip() else {
        return;
    };
    // Dedicated topic so the flood doesn't pollute the shared
    // TEST_KAFKA_TOPIC other tests read back from.
    let topic = format!("voice-bird-e2e-busy-{}", uuid::Uuid::new_v4());

    // Background flooder: fire-and-forget produces as fast as the
    // local queue accepts them. FutureProducer wraps a threaded
    // poller, so dropped delivery futures still get delivered.
    let stop = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop = Arc::clone(&stop);
        let broker = broker.clone();
        let topic = topic.clone();
        std::thread::spawn(move || {
            let producer: rdkafka::producer::FutureProducer = ClientConfig::new()
                .set("bootstrap.servers", &broker)
                .set("message.timeout.ms", "5000")
                .create()
                .expect("create flooder producer");
            let mut sent: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let key = sent.to_string();
                let record = rdkafka::producer::FutureRecord::<str, [u8]>::to(&topic)
                    .key(&key)
                    .payload(b"{\"filler\":true}");
                // Enqueue-only; back off briefly when the local
                // queue is full so the loop doesn't spin.
                if producer.send_result(record).is_err() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                sent += 1;
            }
            sent
        })
    };

    let conn = KafkaAgentConnection {
        endpoint: broker,
        topic: topic.clone(),
        client_id: None,
        acks: Default::default(),
    };
    let target = KafkaTarget::new(AgentSessionId("busy-verify".into()), conn);

    // Verify #1: topic is being auto-created under concurrent load.
    let t1 = target.clone();
    let first = block_on(async move { t1.verify().await });
    // Verify #2: topic now exists and is busy — the exact window the
    // watermark-based assign raced in.
    let t2 = target.clone();
    let second = block_on(async move { t2.verify().await });

    stop.store(true, Ordering::Relaxed);
    let sent = flooder.join().expect("join flooder thread");
    eprintln!("verify_busy_topic: flooder sent ~{sent} records");

    let e1 = first.unwrap_or_else(|e| {
        panic!("verify #1 (auto-create under load) on '{topic}' must succeed: {e}")
    });
    let e2 = second.unwrap_or_else(|e| {
        panic!("verify #2 (established busy topic) on '{topic}' must succeed: {e}")
    });
    eprintln!("verify_busy_topic: OK in {e1:?} then {e2:?}");
}
