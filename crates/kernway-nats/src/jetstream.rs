//! JetStream: streams, durable pull consumers, confirmed publishes and
//! acks — and the [`kernway_messaging::Broker`] backend on top.
//!
//! Mapping of the contract: a **topic** is a stream (named after the topic,
//! sanitized) capturing the subject of the same name; a **group** is a
//! durable pull consumer with explicit acks, its members pulling from it.
//! `publish` waits for the stream's `PubAck`; `ack` is a double ack (the
//! server confirms it); `nack` is `-NAK` (requeue) or `+TERM` (reject — the
//! message is first published to the dead-letter topic, since JetStream has
//! no dead-letter routing of its own); `close` NAKs what the member still
//! holds so other members get it at once.

use crate::client::{Client, Msg, Subscriber};
use crate::options::Options;
use kernway_messaging::{Broker, Capabilities, Consumer, Delivery, MsgError, MsgResult, OutMessage, Producer, Receipt, StartAt, Subscription};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// The header carrying a message's key (base64: keys are bytes).
pub const KEY_HEADER: &str = "Kw-Key";

/// How long one pull request waits on the server for messages.
const PULL_EXPIRES: Duration = Duration::from_secs(1);

/// A JetStream API error.
fn api_error(v: &Value) -> Option<(u64, u64, String)> {
    let e = v.get("error")?;
    Some((
        e.get("code").and_then(Value::as_u64).unwrap_or(0),
        e.get("err_code").and_then(Value::as_u64).unwrap_or(0),
        e.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
    ))
}

/// The stream name for a topic: `.`, `*`, `>`, whitespace and path
/// separators are not allowed in names.
pub fn stream_name(topic: &str) -> String {
    topic.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// JetStream on a [`Client`].
#[derive(Clone)]
pub struct JetStream {
    client: Client,
    prefix: String,
    timeout: Duration,
}

impl JetStream {
    /// JetStream over `client`.
    pub fn new(client: Client) -> Self {
        let prefix = client.options().js_prefix.clone();
        let timeout = client.options().request_timeout;
        JetStream { client, prefix, timeout }
    }

    /// The core client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// A JetStream API call: `$JS.API.<api>` with a JSON body; API errors
    /// come back as [`MsgError::Broker`] with their code.
    pub async fn api(&self, api: &str, body: &Value) -> MsgResult<Value> {
        let payload = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        let m = self.client.request(&format!("{}.{api}", self.prefix), &[], &payload, self.timeout).await.map_err(|e| match e {
            MsgError::Broker(m) if m.starts_with("no responders") => MsgError::Broker(format!("JetStream is not enabled on the server ({m})")),
            e => e,
        })?;
        let v: Value = serde_json::from_slice(&m.payload).map_err(|e| MsgError::Protocol(format!("JetStream {api}: bad reply: {e}")))?;
        if let Some((code, err, desc)) = api_error(&v) {
            return Err(MsgError::Broker(format!("JetStream {api}: {desc} (code {code}, err_code {err})")));
        }
        Ok(v)
    }

    /// Account information (limits, usage).
    pub async fn account_info(&self) -> MsgResult<Value> {
        self.api("INFO", &Value::Null).await
    }

    /// Create a stream (`config` as the server takes it), or keep the
    /// existing one of that name.
    pub async fn create_stream(&self, config: &Value) -> MsgResult<Value> {
        let name = config.get("name").and_then(Value::as_str).ok_or_else(|| MsgError::Config("stream config without name".into()))?;
        match self.api(&format!("STREAM.CREATE.{name}"), config).await {
            Err(MsgError::Broker(m)) if m.contains("err_code 10058") => self.stream_info(name).await,
            r => r,
        }
    }

    /// A stream's configuration and state.
    pub async fn stream_info(&self, name: &str) -> MsgResult<Value> {
        self.api(&format!("STREAM.INFO.{name}"), &Value::Null).await
    }

    /// Delete a stream (absent is fine).
    pub async fn delete_stream(&self, name: &str) -> MsgResult<()> {
        match self.api(&format!("STREAM.DELETE.{name}"), &Value::Null).await {
            Ok(_) => Ok(()),
            Err(MsgError::Broker(m)) if m.contains("err_code 10059") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Remove every message of a stream.
    pub async fn purge_stream(&self, name: &str) -> MsgResult<u64> {
        let v = self.api(&format!("STREAM.PURGE.{name}"), &json!({})).await?;
        Ok(v.get("purged").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Create or update a durable consumer on `stream`.
    pub async fn create_consumer(&self, stream: &str, config: &Value) -> MsgResult<Value> {
        let name = config.get("durable_name").and_then(Value::as_str).ok_or_else(|| MsgError::Config("consumer config without durable_name".into()))?;
        self.api(&format!("CONSUMER.CREATE.{stream}.{name}"), &json!({ "stream_name": stream, "config": config })).await
    }

    /// A consumer's state.
    pub async fn consumer_info(&self, stream: &str, name: &str) -> MsgResult<Value> {
        self.api(&format!("CONSUMER.INFO.{stream}.{name}"), &Value::Null).await
    }

    /// Delete a consumer.
    pub async fn delete_consumer(&self, stream: &str, name: &str) -> MsgResult<()> {
        self.api(&format!("CONSUMER.DELETE.{stream}.{name}"), &Value::Null).await.map(|_| ())
    }

    /// Publish and wait for the stream's acknowledgement: `(stream, seq,
    /// duplicate)`. `msg_id` makes the server drop a repeat inside the
    /// stream's duplicate window.
    pub async fn publish(&self, subject: &str, headers: &[(String, String)], payload: &[u8], msg_id: Option<&str>) -> MsgResult<(String, u64, bool)> {
        let mut h = headers.to_vec();
        if let Some(id) = msg_id {
            h.push(("Nats-Msg-Id".into(), id.into()));
        }
        let m = self.client.request(subject, &h, payload, self.timeout).await.map_err(|e| match e {
            MsgError::Broker(m) if m.starts_with("no responders") => MsgError::Broker(format!("no stream captures subject {subject:?} ({m})")),
            e => e,
        })?;
        let v: Value = serde_json::from_slice(&m.payload).map_err(|e| MsgError::Protocol(format!("bad PubAck: {e}")))?;
        if let Some((code, err, desc)) = api_error(&v) {
            return Err(MsgError::Broker(format!("publish to {subject}: {desc} (code {code}, err_code {err})")));
        }
        Ok((
            v.get("stream").and_then(Value::as_str).unwrap_or("").to_string(),
            v.get("seq").and_then(Value::as_u64).unwrap_or(0),
            v.get("duplicate").and_then(Value::as_bool).unwrap_or(false),
        ))
    }

    /// The last message on `subject` in `stream` (direct read of the stored message).
    pub async fn last_message(&self, stream: &str, subject: &str) -> MsgResult<Option<Value>> {
        match self.api(&format!("STREAM.MSG.GET.{stream}"), &json!({ "last_by_subj": subject })).await {
            Ok(v) => Ok(v.get("message").cloned()),
            Err(MsgError::Broker(m)) if m.contains("err_code 10037") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The metadata in a JetStream ack subject:
/// `$JS.ACK[.<domain>.<account hash>].<stream>.<consumer>.<delivered>.<stream seq>.<consumer seq>.<timestamp>.<pending>[.<token>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckInfo {
    /// The stream.
    pub stream: String,
    /// The consumer.
    pub consumer: String,
    /// How many times it was delivered (1: the first time).
    pub delivered: u64,
    /// Its stream sequence.
    pub stream_seq: u64,
    /// Its consumer sequence.
    pub consumer_seq: u64,
    /// Stored at (ns since the epoch).
    pub timestamp_ns: u64,
    /// Messages still pending for the consumer.
    pub pending: u64,
}

/// Parse an ack subject (`None`: not one).
pub fn parse_ack_subject(s: &str) -> Option<AckInfo> {
    let t: Vec<&str> = s.split('.').collect();
    if t.len() < 9 || t[0] != "$JS" || t[1] != "ACK" {
        return None;
    }
    // v1: 9 tokens; v2: 12 (domain, account hash, …, random token) or more.
    let base = if t.len() == 9 { 2 } else { 4 };
    let n = |i: usize| t.get(base + i).and_then(|x| x.parse::<u64>().ok());
    Some(AckInfo {
        stream: t.get(base)?.to_string(),
        consumer: t.get(base + 1)?.to_string(),
        delivered: n(2)?,
        stream_seq: n(3)?,
        consumer_seq: n(4)?,
        timestamp_ns: n(5)?,
        pending: n(6)?,
    })
}

/// The JetStream [`Broker`].
#[derive(Clone)]
pub struct NatsBroker {
    js: JetStream,
    /// Stream storage: `file` (durable, default) or `memory`.
    pub storage: &'static str,
    /// Stream replicas (clustered servers).
    pub replicas: u32,
}

impl NatsBroker {
    /// Connect with options.
    pub async fn connect(opts: Options) -> MsgResult<Self> {
        Ok(Self::new(Client::connect(opts).await?))
    }

    /// Connect with a URL.
    pub async fn connect_url(url: &str) -> MsgResult<Self> {
        Self::connect(Options::parse(url)?).await
    }

    /// The broker over a connected client.
    pub fn new(client: Client) -> Self {
        NatsBroker { js: JetStream::new(client), storage: "file", replicas: 1 }
    }

    /// The JetStream handle.
    pub fn jetstream(&self) -> &JetStream {
        &self.js
    }
}

/// Publishes to JetStream.
pub struct NatsProducer {
    js: JetStream,
}

fn key_headers(msg: &OutMessage) -> Vec<(String, String)> {
    let mut h = msg.headers.clone();
    if let Some(k) = &msg.key {
        h.push((KEY_HEADER.into(), kernway_wire::scram::b64_encode(k)));
    }
    h
}

impl Producer for NatsProducer {
    async fn publish(&self, topic: &str, msg: OutMessage) -> MsgResult<Receipt> {
        let (stream, seq, _) = self.js.publish(topic, &key_headers(&msg), &msg.payload, None).await?;
        Ok(Receipt { id: format!("{stream}:{seq}") })
    }
}

/// A member of a group: pulls from the group's durable consumer.
pub struct NatsConsumer {
    js: JetStream,
    sub: Subscription,
    stream: String,
    durable: String,
    inbox: String,
    rx: Subscriber,
    /// Messages still due on the outstanding pull request (0: none open),
    /// and when that request expires.
    due: usize,
    due_until: Instant,
    /// Deliveries this member holds (ack subjects).
    held: Vec<String>,
}

impl NatsConsumer {
    fn pull(&mut self) -> MsgResult<()> {
        let batch = self.sub.max_in_flight.saturating_sub(self.held.len()).max(1);
        let body = json!({ "batch": batch, "expires": PULL_EXPIRES.as_nanos() as u64 }).to_string();
        self.js.client.publish(&format!("{}.CONSUMER.MSG.NEXT.{}.{}", self.js.prefix, self.stream, self.durable), Some(&self.inbox), &[], body.as_bytes())?;
        self.due = batch;
        self.due_until = Instant::now() + PULL_EXPIRES + Duration::from_millis(500);
        Ok(())
    }

    fn to_delivery(&self, m: Msg) -> Option<Delivery> {
        let reply = m.reply?;
        let info = parse_ack_subject(&reply)?;
        let mut key = None;
        let mut headers = Vec::with_capacity(m.headers.len());
        for (k, v) in m.headers {
            if k.eq_ignore_ascii_case(KEY_HEADER) && key.is_none() {
                key = kernway_wire::scram::b64_decode(&v);
            } else {
                headers.push((k, v));
            }
        }
        Some(Delivery { topic: m.subject, key, payload: m.payload, headers, redelivered: info.delivered > 1, tag: reply })
    }

    /// Settle with the server's confirmation (double ack).
    async fn settle(&mut self, d: &Delivery, what: &[u8]) -> MsgResult<()> {
        self.held.retain(|t| *t != d.tag);
        self.js.client.request(&d.tag, &[], what, self.js.timeout).await.map(|_| ())
    }

    /// Wait until the open pull request is over, NAKing what still arrives.
    async fn finish_pull(&mut self) {
        while self.due > 0 && Instant::now() < self.due_until {
            let wait = self.due_until.saturating_duration_since(Instant::now());
            match self.rx.next_timeout(wait).await {
                Ok(m) if m.status.is_some() => self.due = 0,
                Ok(m) => {
                    self.due -= 1;
                    if let Some(r) = m.reply {
                        let _ = self.js.client.publish(&r, None, &[], b"-NAK");
                    }
                }
                Err(_) => break,
            }
        }
        self.due = 0;
    }
}

impl Consumer for NatsConsumer {
    async fn next(&mut self) -> MsgResult<Delivery> {
        loop {
            if self.due == 0 || Instant::now() >= self.due_until {
                if self.held.len() >= self.sub.max_in_flight {
                    // At the in-flight limit: wait for acks (the caller's job).
                    rt_core::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                self.pull()?;
            }
            let wait = self.due_until.saturating_duration_since(Instant::now());
            let m = match self.rx.next_timeout(wait).await {
                Ok(m) => m,
                Err(MsgError::Timeout) => {
                    self.due = 0;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if let Some((code, desc)) = &m.status {
                match code {
                    // No messages / request expired / batch completed: pull again.
                    404 | 408 => self.due = 0,
                    // Leadership change, consumer deleted, max waiting …
                    409 => {
                        self.due = 0;
                        if desc.contains("Consumer Deleted") {
                            return Err(MsgError::Broker(format!("consumer {} was deleted", self.durable)));
                        }
                        rt_core::sleep(Duration::from_millis(50)).await;
                    }
                    // Idle heartbeat / flow control.
                    100 => {}
                    _ => {
                        self.due = 0;
                        return Err(MsgError::Broker(format!("pull from {}: {code} {desc}", self.durable)));
                    }
                }
                continue;
            }
            self.due = self.due.saturating_sub(1);
            if let Some(d) = self.to_delivery(m) {
                self.held.push(d.tag.clone());
                return Ok(d);
            }
        }
    }

    async fn ack(&mut self, d: &Delivery) -> MsgResult<()> {
        self.settle(d, b"+ACK").await
    }

    async fn nack(&mut self, d: &Delivery, requeue: bool) -> MsgResult<()> {
        if requeue {
            return self.settle(d, b"-NAK").await;
        }
        if let Some(dl) = self.sub.dead_letter.clone() {
            let mut h = d.headers.clone();
            if let Some(k) = &d.key {
                h.push((KEY_HEADER.into(), kernway_wire::scram::b64_encode(k)));
            }
            h.push(("Kw-Dead-Letter-From".into(), d.topic.clone()));
            self.js.publish(&dl, &h, &d.payload, None).await?;
        }
        self.settle(d, b"+TERM").await
    }

    async fn close(mut self) -> MsgResult<()> {
        self.finish_pull().await;
        for tag in std::mem::take(&mut self.held) {
            self.js.client.publish(&tag, None, &[], b"-NAK")?;
        }
        self.js.client.flush().await
    }
}

impl Drop for NatsConsumer {
    fn drop(&mut self) {
        // Not closed: hand back what is held (best effort).
        for tag in std::mem::take(&mut self.held) {
            let _ = self.js.client.publish(&tag, None, &[], b"-NAK");
        }
    }
}

impl Broker for NatsBroker {
    type Producer = NatsProducer;
    type Consumer = NatsConsumer;

    async fn producer(&self) -> MsgResult<NatsProducer> {
        Ok(NatsProducer { js: self.js.clone() })
    }

    async fn subscribe(&self, sub: Subscription) -> MsgResult<NatsConsumer> {
        self.ensure_topic(&sub.topic).await?;
        let stream = stream_name(&sub.topic);
        let durable = stream_name(&sub.group);
        let config = json!({
            "durable_name": durable,
            "ack_policy": "explicit",
            "deliver_policy": if sub.start == StartAt::Earliest { "all" } else { "new" },
            "ack_wait": sub.ack_wait.as_nanos() as u64,
            "max_deliver": -1,
            "max_ack_pending": 10_000,
            "max_waiting": 512,
            "filter_subject": sub.topic,
        });
        match self.js.create_consumer(&stream, &config).await {
            Ok(_) => {}
            // It exists with another start / settings: join it as it is.
            Err(MsgError::Broker(m)) if m.contains("err_code 10148") || m.contains("err_code 10012") || m.contains("err_code 10105") => {
                self.js.consumer_info(&stream, &durable).await?;
            }
            Err(e) => return Err(e),
        }
        let inbox = self.js.client.new_inbox();
        let rx = self.js.client.subscribe(&inbox, None)?;
        Ok(NatsConsumer { js: self.js.clone(), sub, stream, durable, inbox, rx, due: 0, due_until: Instant::now(), held: Vec::new() })
    }

    async fn ensure_topic(&self, topic: &str) -> MsgResult<()> {
        let config = json!({
            "name": stream_name(topic),
            "subjects": [topic],
            "retention": "limits",
            "storage": self.storage,
            "num_replicas": self.replicas,
            "discard": "old",
            "duplicate_window": 120_000_000_000u64,
        });
        self.js.create_stream(&config).await.map(|_| ())
    }

    async fn delete_topic(&self, topic: &str) -> MsgResult<()> {
        self.js.delete_stream(&stream_name(topic)).await
    }

    fn capabilities(&self) -> Capabilities {
        // One stream, consumed by every member: a key's messages may be in
        // flight at two members at once.
        Capabilities { replay: true, ack_timeout: true, dead_letter: true, key_order_across_members: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_subjects() {
        let v1 = parse_ack_subject("$JS.ACK.ORDERS.workers.2.17.5.1700000000000000000.3").unwrap();
        assert_eq!((v1.stream.as_str(), v1.consumer.as_str(), v1.delivered, v1.stream_seq, v1.consumer_seq, v1.pending), ("ORDERS", "workers", 2, 17, 5, 3));
        let v2 = parse_ack_subject("$JS.ACK.hub.ACCHASH.ORDERS.workers.1.9.9.1700000000000000000.0.tok").unwrap();
        assert_eq!((v2.stream.as_str(), v2.delivered, v2.stream_seq), ("ORDERS", 1, 9));
        assert!(parse_ack_subject("_INBOX.x").is_none());
        assert_eq!(stream_name("pos.events>*"), "pos_events__");
    }
}
