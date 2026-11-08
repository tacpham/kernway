//! Server-Sent Events (SSE) — W3C EventSource specification.
//!
//! SSE allows servers to push data to clients over a persistent HTTP connection.
//! The connection stays open until either side closes it.
//!
//! # Example
//! ```rust,ignore
//! .get("/events", |_req, _ctx| {
//!     SseStream::new(vec![
//!         SseEvent::data("connected"),
//!         SseEvent::with_id("1", "message", "Hello from server"),
//!     ])
//!     .into_response()
//! })
//! ```

use kernway_core::{
    error::StatusCode,
    response::{IntoResponse, Response},
};

/// A single Server-Sent Event.
///
/// Format (from W3C spec):
/// ```text
/// id: <id>\n
/// event: <event_type>\n
/// data: <data>\n
/// retry: <ms>\n
/// \n
/// ```
#[derive(Debug, Clone, Default)]
pub struct SseEvent {
    /// Optional event ID (allows client to resume from last seen ID).
    pub id: Option<String>,
    /// Event type (default: "message").
    pub event: Option<String>,
    /// The data payload. Multi-line data is split across multiple `data:` lines.
    pub data: String,
    /// Tell the client how long to wait before reconnecting (milliseconds).
    pub retry: Option<u64>,
}

impl SseEvent {
    /// Create a simple data-only event.
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            ..Default::default()
        }
    }

    /// Create an event with id, type, and data.
    pub fn with_id(
        id: impl Into<String>,
        event: impl Into<String>,
        data: impl Into<String>,
    ) -> Self {
        Self {
            id: Some(id.into()),
            event: Some(event.into()),
            data: data.into(),
            retry: None,
        }
    }

    /// Create a named event (no ID).
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
            ..Default::default()
        }
    }

    /// Set reconnection retry interval in milliseconds.
    pub fn retry(mut self, ms: u64) -> Self {
        self.retry = Some(ms);
        self
    }

    /// Serialize to SSE wire format.
    pub fn to_wire(&self) -> String {
        let mut out = String::new();
        if let Some(id) = &self.id {
            out.push_str(&format!("id: {}\n", id));
        }
        if let Some(event) = &self.event {
            out.push_str(&format!("event: {}\n", event));
        }
        for line in self.data.lines() {
            out.push_str(&format!("data: {}\n", line));
        }
        if self.data.is_empty() {
            out.push_str("data: \n");
        }
        if let Some(retry) = self.retry {
            out.push_str(&format!("retry: {}\n", retry));
        }
        out.push('\n');
        out
    }
}

/// SSE response — holds a list of events to send.
///
/// In thread-per-connection model, the entire event list is serialized
/// and sent as a single response body. For true streaming (server keeps
/// pushing), use `SseStream::from_fn` with a closure that yields events
/// until it returns `None`.
///
/// Note: true long-lived SSE with arbitrary push timing requires async I/O.
/// This sync implementation is suitable for "batch push" scenarios.
pub struct SseStream {
    events: Vec<SseEvent>,
}

impl SseStream {
    /// Create a stream from a fixed list of events.
    pub fn new(events: Vec<SseEvent>) -> Self {
        Self { events }
    }

    /// Serialize all events to SSE wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let body: String = self.events.iter().map(|e| e.to_wire()).collect();
        body.into_bytes()
    }
}

impl IntoResponse for SseStream {
    fn into_response(self) -> Response {
        let body = self.to_bytes();
        Response::new(StatusCode::OK)
            .content_type("text/event-stream; charset=utf-8")
            .body(body)
    }
}

// --- Live (open-ended) SSE ---------------------------------------------------

use kernway_cache_core::pubsub::Subscription;
use kernway_core::layer::BoxFuture;
use kernway_core::response::BodyStream;

/// An async source of SSE events for a live stream: `next` yields the next event,
/// or `None` to end the stream.
pub trait SseSource {
    /// The next event to push, or `None` when the stream is finished.
    fn next(&mut self) -> BoxFuture<'_, Option<SseEvent>>;
}

/// A live, **open-ended** SSE response: events are pulled from an [`SseSource`]
/// and written as they arrive over a chunked body, until the source ends or the
/// client disconnects. Pair it with a message-bus subscription
/// ([`LiveSse::from_subscription`]) for cross-node real-time fan-out — a POS
/// kiosk check-in on one node reaching a front-desk screen on another.
///
/// ```rust,ignore
/// .get("/location/42/events", |_req, scope| async move {
///     let sub = bus.subscribe(&["pos:location:42"]).await?;
///     LiveSse::from_subscription(sub).into_response()
/// })
/// ```
pub struct LiveSse {
    source: Box<dyn SseSource>,
}

impl LiveSse {
    /// Build a live SSE stream from any event source.
    pub fn new(source: Box<dyn SseSource>) -> Self {
        Self { source }
    }

    /// Build a live SSE stream from a pub/sub [`Subscription`]: each message
    /// becomes a `data:` event carrying its payload.
    pub fn from_subscription(subscription: Box<dyn Subscription>) -> Self {
        Self::new(Box::new(SubscriptionSource { subscription }))
    }
}

impl BodyStream for LiveSse {
    fn next(&mut self) -> BoxFuture<'_, std::io::Result<Option<Vec<u8>>>> {
        Box::pin(async move { Ok(self.source.next().await.map(|ev| ev.to_wire().into_bytes())) })
    }
}

impl IntoResponse for LiveSse {
    fn into_response(self) -> Response {
        let mut resp =
            Response::new(StatusCode::OK).content_type("text/event-stream; charset=utf-8");
        // Prevent proxy/browser buffering so events arrive promptly.
        resp.headers.insert("cache-control", "no-cache");
        resp.chunked(Box::new(self))
    }
}

/// Adapts a pub/sub [`Subscription`] into an [`SseSource`]: each received message
/// is emitted as a `data:` event (payload decoded lossily as UTF-8). A receive
/// error ends the stream.
struct SubscriptionSource {
    subscription: Box<dyn Subscription>,
}

impl SseSource for SubscriptionSource {
    fn next(&mut self) -> BoxFuture<'_, Option<SseEvent>> {
        Box::pin(async move {
            match self.subscription.recv().await {
                Ok(msg) => Some(SseEvent::data(String::from_utf8_lossy(&msg.payload).into_owned())),
                Err(_) => None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_data_event_format() {
        let e = SseEvent::data("hello");
        assert_eq!(e.to_wire(), "data: hello\n\n");
    }

    #[test]
    fn event_with_id_and_type() {
        let e = SseEvent::with_id("42", "update", "payload");
        let wire = e.to_wire();
        assert!(wire.contains("id: 42\n"));
        assert!(wire.contains("event: update\n"));
        assert!(wire.contains("data: payload\n"));
        assert!(wire.ends_with("\n\n"));
    }

    #[test]
    fn retry_field() {
        let e = SseEvent::data("ping").retry(3000);
        assert!(e.to_wire().contains("retry: 3000\n"));
    }

    #[test]
    fn multiline_data_split_into_multiple_data_lines() {
        let e = SseEvent::data("line1\nline2\nline3");
        let wire = e.to_wire();
        assert!(wire.contains("data: line1\n"));
        assert!(wire.contains("data: line2\n"));
        assert!(wire.contains("data: line3\n"));
    }

    #[test]
    fn empty_data_still_emits_data_line() {
        let e = SseEvent::data("");
        assert!(e.to_wire().contains("data: \n"));
    }

    #[test]
    fn sse_stream_into_response_content_type() {
        let resp = SseStream::new(vec![SseEvent::data("x")]).into_response();
        assert_eq!(resp.status.0, 200);
        assert!(resp
            .headers
            .get("content-type")
            .unwrap()
            .contains("text/event-stream"));
    }

    #[test]
    fn sse_stream_body_contains_all_events() {
        let resp = SseStream::new(vec![SseEvent::data("event1"), SseEvent::data("event2")])
            .into_response();
        let body = String::from_utf8(resp.body_bytes().to_vec()).unwrap();
        assert!(body.contains("data: event1"));
        assert!(body.contains("data: event2"));
    }

    #[test]
    fn named_event() {
        let e = SseEvent::named("heartbeat", "{}");
        let wire = e.to_wire();
        assert!(wire.contains("event: heartbeat\n"));
        assert!(wire.contains("data: {}\n"));
        assert!(!wire.contains("id:"));
    }

    // --- live SSE ---

    use kernway_cache_core::pubsub::{Message, Subscription};
    use kernway_cache_core::CacheError;
    use kernway_core::response::{Body, BodyStream};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    /// Event source that drains a fixed list, then ends.
    struct VecSource(Vec<SseEvent>);
    impl SseSource for VecSource {
        fn next(&mut self) -> BoxFuture<'_, Option<SseEvent>> {
            Box::pin(async move {
                if self.0.is_empty() {
                    None
                } else {
                    Some(self.0.remove(0))
                }
            })
        }
    }

    /// Subscription that yields canned payloads, then errors (stream end).
    struct MockSub {
        msgs: Vec<Vec<u8>>,
    }
    impl Subscription for MockSub {
        fn recv(&mut self) -> BoxFuture<'_, Result<Message, CacheError>> {
            Box::pin(async move {
                if self.msgs.is_empty() {
                    Err(CacheError::Connection("end".into()))
                } else {
                    Ok(Message { channel: "c".into(), payload: self.msgs.remove(0) })
                }
            })
        }
    }

    #[test]
    fn live_sse_yields_wire_events_then_ends() {
        let mut live = LiveSse::new(Box::new(VecSource(vec![
            SseEvent::data("hello"),
            SseEvent::named("turn", "x"),
        ])));
        block_on(async {
            assert_eq!(live.next().await.unwrap(), Some(b"data: hello\n\n".to_vec()));
            let c2 = live.next().await.unwrap().unwrap();
            assert_eq!(String::from_utf8(c2).unwrap(), "event: turn\ndata: x\n\n");
            assert_eq!(live.next().await.unwrap(), None);
        });
    }

    #[test]
    fn subscription_becomes_data_events() {
        let mut live = LiveSse::from_subscription(Box::new(MockSub {
            msgs: vec![b"checkin:Jen".to_vec()],
        }));
        block_on(async {
            let c = live.next().await.unwrap().unwrap();
            assert_eq!(String::from_utf8(c).unwrap(), "data: checkin:Jen\n\n");
            // next recv errors → stream ends
            assert_eq!(live.next().await.unwrap(), None);
        });
    }

    #[test]
    fn live_sse_into_response_is_chunked_event_stream() {
        let resp = LiveSse::new(Box::new(VecSource(vec![]))).into_response();
        assert_eq!(resp.status.0, 200);
        assert!(resp.headers.get("content-type").unwrap().contains("text/event-stream"));
        assert!(resp.headers.get("cache-control").unwrap().contains("no-cache"));
        assert!(matches!(resp.body, Body::Chunked { .. }));
    }
}
