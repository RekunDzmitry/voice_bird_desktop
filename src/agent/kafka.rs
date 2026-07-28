//! Kafka-flavoured [`AgentTarget`] implementation.
//!
//! Each user-configured Kafka target owns one `rdkafka::FutureProducer`
//! and a small in-process ring buffer of recent segments (used by
//! `pull_recent`, the agent-catch-up tool). Committed segments are
//! serialised to a single JSON line and produced to the configured
//! `topic` with a stable key (segment start time) so partition
//! assignment stays meaningful.
//!
//! The producer is created lazily on the first `push_segment` call,
//! not at target-construction time. That keeps the funnel's
//! "verify connection" step responsible for actually opening a
//! socket, and avoids holding a TCP connection open for the lifetime
//! of the TUI for targets the user may never record against.
//!
//! The trait's [`AgentTarget::push_segment`] is sync; rdkafka's
//! `send` returns an async future. The Kafka target exposes
//! `pub async fn push_segment_async` and a sync wrapper
//! that spawns a fresh thread+runtime so the trait impl
//! doesn't deadlock the caller's tokio runtime.
use parking_lot::Mutex;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::message::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::util::Timeout;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::session::{AgentSessionId, AgentTarget};
use crate::config::KafkaAgentConnection;
use crate::transcription::Segment;

/// How long `push_segment` waits for the broker to ack a record
/// before giving up. The trait says errors are best-effort and
/// non-fatal; a 5 s ceiling matches the `acks=all` path and keeps
/// a stalled broker from blocking the recorder indefinitely.
const PRODUCE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`KafkaTarget::verify`] waits for the round-trip
/// (produce + consume) probe to land. Generous on purpose: cold
/// broker bring-up + topic auto-create can take a couple of seconds
/// on a single-node test cluster.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-target ring buffer size. Same shape as the MCP-server buffer
/// in [`crate::agent::mcp_server`] so consumers get a consistent
/// catch-up window regardless of which Agent target is in use.
const BUFFER_CAP: usize = 10_000;

/// Kafka-flavoured [`AgentTarget`]. Cheap to clone — the inner
/// `FutureProducer` is `Clone` (it wraps an `Arc`) and the
/// segment buffer is parked behind a `Mutex` on a single owner
/// that lives inside the `Arc` so clones share state.
#[derive(Clone)]
pub struct KafkaTarget {
    inner: Arc<KafkaTargetInner>,
}

struct KafkaTargetInner {
    /// Stable id from the user-facing config. The session id
    /// exposed to agents resolves to this value when they
    /// `pull_recent` so multiple Kafka targets stay distinguishable
    /// in their consumer logs.
    session: AgentSessionId,
    /// Connection the target was built from. Held so `verify`
    /// can rebuild a temporary producer without the caller
    /// having to plumb the config through again.
    connection: KafkaAgentConnection,
    /// eagerly initialised in `verify`). Wrapped in a
    /// `Mutex<Option<_>>` so the `verify` path can also drop
    /// a stale producer that the user just changed the
    /// connection details on.
    producer: Mutex<Option<FutureProducer>>,
    /// Bounded ring buffer of segments pushed so far, oldest at
    /// the front. `pull_recent` reads the back; `push_segment`
    /// appends and evicts the front when the cap is hit.
    /// `VecDeque` so the eviction is O(1) instead of the
    /// O(n) shift `Vec::remove(0)` would cost on a full
    /// buffer.
    buffer: Mutex<VecDeque<Segment>>,
}

impl KafkaTarget {
    /// Build a target that won't actually open a connection until
    /// the first segment is pushed (or `verify` is called). Cheap
    /// — safe to construct at config-load time.
    pub fn new(id: impl Into<AgentSessionId>, connection: KafkaAgentConnection) -> Self {
        let session = id.into();
        Self {
            inner: Arc::new(KafkaTargetInner {
                session,
                connection,
                producer: Mutex::new(None),
                buffer: Mutex::new(VecDeque::with_capacity(BUFFER_CAP)),
            }),
        }
    }

    /// Connection this target was configured with. Used by the funnel
    /// to re-verify after the user edits the form and to log which
    /// broker/topic a push failed against.
    pub fn connection(&self) -> &KafkaAgentConnection {
        &self.inner.connection
    }

    /// Build (or rebuild) the `FutureProducer` from the saved
    /// connection details. Any previous producer is dropped, which
    /// lets the funnel re-verify cleanly after an edit.
    fn producer(&self) -> Result<FutureProducer, KafkaError> {
        let mut slot = self.inner.producer.lock();
        if let Some(p) = slot.as_ref() {
            return Ok(p.clone());
        }
        let producer = build_producer(
            &self.inner.connection,
            &self.inner.session,
            PRODUCE_TIMEOUT,
            self.inner.connection.acks,
        )?;
        *slot = Some(producer.clone());
        Ok(producer)
    }

    /// Verify the configured connection end-to-end: build a fresh
    /// producer, produce a small JSON probe with a unique id, then
    /// consume from the start of the topic (via a fresh
    /// `StreamConsumer`) until the probe id shows up. Returns the
    /// elapsed time on success; returns `Err` with the broker
    /// reason on any failure.
    pub async fn verify(&self) -> anyhow::Result<Duration> {
        // Drop any cached producer so we re-test the live config.
        *self.inner.producer.lock() = None;
        let started = Instant::now();
        // Build a one-shot producer with `message.timeout.ms =
        // VERIFY_TIMEOUT` so the probe can actually use the
        // generous window the constant advertises (the cached
        // producer uses PRODUCE_TIMEOUT = 5 s, which would cap
        // delivery well before our 15 s deadline). `acks` is
        // pinned to `all` regardless of the target's setting:
        // verify's whole point is confirming the broker
        // persisted a record, and the delivery report only
        // carries a real offset when the broker actually acks
        // (`acks=0` fakes the report with offset -1, which
        // would break the assign below). The target's own
        // `acks` still governs real segment pushes.
        let producer = build_producer(
            &self.inner.connection,
            &self.inner.session,
            VERIFY_TIMEOUT,
            crate::config::KafkaAcks::All,
        )?;

        // Probe id is a fresh UUID so two concurrent verifies (or
        // a verify + a real push) can't collide on the same key.
        let probe_id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::json!({
            "voice_bird_agent_verify": true,
            "probe_id": probe_id,
            "session": self.inner.session.as_str(),
        });
        let probe_bytes = serde_json::to_vec(&payload)?;
        let key = probe_id.clone();
        let topic = self.inner.connection.topic.clone();
        // Skip the consumer-group machinery entirely: a fresh
        // group takes seconds to JoinGroup/SyncGroup
        // (`group.initial.rebalance.delay.ms` defaults to 3 s on
        // the broker) and `auto.offset.reset = latest` would
        // resolve the "log end" at *assignment* time, so the
        // probe we produce right after would already be behind
        // us and the consumer would never see it. `assign()`
        // with an explicit offset is a synchronous local command
        // — fast and race-free. The probe lands at
        // `high - 1` on its partition, and we replay every
        // partition from `high - 1` so the consumer is
        // always positioned at or before the probe.
        let consumer = build_consumer(
            &self.inner.connection,
            &self.inner.session,
            // Group id is required by librdkafka even when
            // using `assign()` directly. Use a fresh UUID
            // anyway so we never share offsets with another
            // live consumer.
            &format!("voice-bird-cli-verify-{}", uuid::Uuid::new_v4()),
            // auto.offset.reset is irrelevant when we `assign()`
            // explicit offsets, but pass `latest` as a
            // belt-and-braces default in case the broker
            // rewinds the assignment.
            "latest",
        )?;
        // Produce the probe. The produce is the canonical
        // "is this broker reachable AND can it accept a record
        // for this topic?" check: it covers connection,
        // authorization, and topic auto-create in one shot.
        // The metadata-first ordering we had before was a
        // correctly-faster path on pre-existing topics but
        // raced against auto-create on fresh ones (R6), and
        // the actionable error message for the auto-create-off
        // case never reached the user (the fallback swallowed
        // it). Producing first gives one code path and one
        // honest error stream.
        //
        // The delivery report is the whole trick: with
        // `acks=all` the resolved future carries the exact
        // `(partition, offset)` the broker wrote the probe to,
        // so the consumer can `assign()` precisely there — no
        // metadata fetch, no watermark round-trips, and no
        // window for concurrent producers on a busy topic to
        // push the head past the probe between produce and
        // assign (the watermark approach raced exactly there).
        let record: FutureRecord<'_, String, Vec<u8>> =
            FutureRecord::to(&topic).key(&key).payload(&probe_bytes);
        let (probe_partition, probe_offset) = producer
            .send(record, Timeout::After(VERIFY_TIMEOUT))
            .await
            .map_err(|(e, _msg)| {
                anyhow::anyhow!(
                    "produce probe failed: {e} (broker={}). \
                     If the broker has auto.create.topics.enable=false, \
                     create the topic '{topic}' manually first.",
                    self.inner.connection.endpoint
                )
            })?;
        // `acks=all` is pinned on the verify producer, so a
        // negative offset here means the broker (or a proxy in
        // front of it) is not reporting offsets — bail with a
        // real error rather than assigning a garbage position.
        debug_assert!(
            probe_offset >= 0,
            "delivery report returned offset {probe_offset} with acks=all"
        );
        if probe_offset < 0 {
            anyhow::bail!(
                "verify: broker acked the probe but reported no offset \
                 (partition={probe_partition}, offset={probe_offset}) — \
                 cannot position the read-back consumer"
            );
        }
        let mut tpl = rdkafka::TopicPartitionList::new();
        tpl.add_partition_offset(
            &topic,
            probe_partition,
            rdkafka::Offset::Offset(probe_offset),
        )?;
        consumer.assign(&tpl)?;

        // Poll until we see the probe id or the timeout elapses.
        let deadline = Instant::now() + VERIFY_TIMEOUT;
        let topic_for_loop = topic.clone();
        let probe_id_for_loop = probe_id.clone();
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), consumer.recv()).await {
                Ok(Ok(msg)) => {
                    if let Some(payload) = msg.payload() {
                        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) {
                            if value.get("probe_id").and_then(|v| v.as_str())
                                == Some(&probe_id_for_loop)
                            {
                                return Ok(started.elapsed());
                            }
                        }
                    }
                }
                // Either the inner poll returned PartitionEOF
                // (skip — keep going) or our 500 ms wall-clock
                // timeout fired (also skip — keep going).
                Ok(Err(rdkafka::error::KafkaError::PartitionEOF(_))) | Err(_) => continue,
                Ok(Err(e)) => {
                    return Err(anyhow::anyhow!(
                        "consume probe failed: {e} (broker={})",
                        self.inner.connection.endpoint
                    ));
                }
            }
        }
        Err(anyhow::anyhow!(
            "verify timed out after {:?} without seeing probe id on '{}'",
            VERIFY_TIMEOUT,
            topic_for_loop
        ))
    }

    /// Push a `Segment` to the configured broker + topic. Async
    /// sibling of [`AgentTarget::push_segment`] — the trait
    /// implementation calls this from a sync context via
    /// [`block_on_current`].
    pub async fn push_segment_async(&self, segment: &Segment) -> anyhow::Result<()> {
        let producer = self.producer()?;

        // Stable partition key: the segment's start time so all
        // segments of one session land on one partition (preserves
        // order for downstream consumers).
        let key = segment.t_start.as_millis().to_string();
        let payload = serde_json::to_vec(segment)?;
        let topic = self.inner.connection.topic.clone();
        let record: FutureRecord<'_, String, Vec<u8>> =
            FutureRecord::to(&topic).key(&key).payload(&payload);

        producer
            .send(record, Timeout::After(PRODUCE_TIMEOUT))
            .await
            .map_err(|(e, _msg)| anyhow::anyhow!("kafka produce failed: {e}"))?;

        // Mirror into the in-process buffer so a late-joining
        // agent can `pull_recent` and catch up. `VecDeque`
        // makes the eviction O(1).
        let mut buf = self.inner.buffer.lock();
        if buf.len() == BUFFER_CAP {
            buf.pop_front();
        }
        buf.push_back(segment.clone());
        Ok(())
    }
}

impl AgentTarget for KafkaTarget {
    fn session_id(&self) -> AgentSessionId {
        self.inner.session.clone()
    }

    fn push_segment(&self, segment: &Segment) -> anyhow::Result<()> {
        // Drive the async producer on a dedicated thread
        // with its own current-thread runtime. The
        // recorder spawns the consumer task on the TUI
        // runtime; using `block_on` from inside that
        // runtime would panic. Spinning a fresh thread
        // here keeps the two runtimes isolated — rdkafka's
        // background tasks need a real reactor, which
        // `block_on` on a `Handle` already provides, but
        // only if the future cooperatively yields and
        // doesn't deadlock the executor. A separate thread
        // sidesteps that risk entirely.
        let seg = segment.clone();
        let this = self.clone();
        let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build one-shot tokio runtime");
            let r = rt.block_on(this.push_segment_async(&seg));
            let _ = tx.send(r);
        });
        // Block until the worker sends its result. The only way
        // `recv` returns Err here is if the worker thread
        // panicked (which also drops the channel) or the OS
        // killed the thread — either way the segment wasn't
        // delivered. Surface that as a per-segment error so the
        // consumer task logs and moves on; we deliberately do
        // NOT panic the recorder for a single misbehaving
        // segment, matching the trait's "best-effort, errors
        // are non-fatal" contract.
        rx.recv()
            .map_err(|e| anyhow::anyhow!("producer worker join failed: {e}"))?
    }

    fn pull_recent(&self, limit: usize) -> Vec<Segment> {
        let buf = self.inner.buffer.lock();
        // VecDeque has no slice-range indexing; iterate from
        // the tail by hand to keep the same "most recent N"
        // ordering as the previous `Vec` implementation.
        let start = buf.len().saturating_sub(limit);
        buf.iter().skip(start).cloned().collect()
    }
}

fn build_producer(
    conn: &KafkaAgentConnection,
    session: &AgentSessionId,
    message_timeout: Duration,
    acks: crate::config::KafkaAcks,
) -> Result<FutureProducer, KafkaError> {
    let mut cfg = ClientConfig::new();
    cfg.set("bootstrap.servers", &conn.endpoint)
        // `acks` is passed explicitly rather than read from
        // `conn`: segment pushes honour the user's setting, but
        // the verify path pins `all` so the delivery report
        // carries a real offset (with `acks=0` librdkafka fakes
        // the report and the offset comes back as -1).
        .set("acks", acks.as_str())
        .set(
            "client.id",
            conn.client_id
                .clone()
                .unwrap_or_else(|| format!("voice-bird-cli/{}", session.as_str())),
        )
        // Connection-level timeouts. The 5 s request timeout pairs
        // with `PRODUCE_TIMEOUT` so a slow broker doesn't pin the
        // recorder forever. `message.timeout.ms` is the total
        // time librdkafka will keep retrying a single message
        // before surfacing an error; for normal pushes we want
        // it short, but the verify path passes VERIFY_TIMEOUT
        // so a slow broker can actually use the generous window
        // the constant promises.
        .set("request.timeout.ms", "5000")
        .set(
            "message.timeout.ms",
            message_timeout.as_millis().to_string(),
        )
        .set("socket.timeout.ms", "5000")
        .set("enable.idempotence", "false");
    cfg.create()
}

fn build_consumer(
    conn: &KafkaAgentConnection,
    session: &AgentSessionId,
    group_id: &str,
    auto_offset_reset: &str,
) -> Result<StreamConsumer, KafkaError> {
    let mut cfg = ClientConfig::new();
    cfg.set("bootstrap.servers", &conn.endpoint)
        // The verify path passes a fresh UUID here so two
        // concurrent verifies (or two app instances) don't
        // join the same group, split partitions, and have
        // one of them miss its own probe. Other callers can
        // pass the session-derived stable id.
        .set("group.id", group_id)
        // `latest` skips the backlog and reads only messages
        // produced after the consumer subscribed — paired
        // with the subscribe-before-produce order in
        // `verify` so the probe is the first thing the
        // consumer can see.
        .set("auto.offset.reset", auto_offset_reset)
        .set("enable.auto.commit", "false")
        .set("session.timeout.ms", "6000")
        .set(
            "client.id",
            conn.client_id
                .clone()
                .unwrap_or_else(|| format!("voice-bird-cli-verify/{}", session.as_str())),
        );
    cfg.create()
}
