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

use kernway_core::{error::StatusCode, response::{IntoResponse, Response}};

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
    pub id:    Option<String>,
    /// Event type (default: "message").
    pub event: Option<String>,
    /// The data payload. Multi-line data is split across multiple `data:` lines.
    pub data:  String,
    /// Tell the client how long to wait before reconnecting (milliseconds).
    pub retry: Option<u64>,
}

impl SseEvent {
    /// Create a simple data-only event.
    pub fn data(data: impl Into<String>) -> Self {
        Self { data: data.into(), ..Default::default() }
    }

    /// Create an event with id, type, and data.
    pub fn with_id(id: impl Into<String>, event: impl Into<String>, data: impl Into<String>) -> Self {
        Self { id: Some(id.into()), event: Some(event.into()), data: data.into(), retry: None }
    }

    /// Create a named event (no ID).
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self { event: Some(event.into()), data: data.into(), ..Default::default() }
    }

    /// Set reconnection retry interval in milliseconds.
    pub fn retry(mut self, ms: u64) -> Self { self.retry = Some(ms); self }

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
        assert!(resp.headers.get("content-type").unwrap().contains("text/event-stream"));
    }

    #[test]
    fn sse_stream_body_contains_all_events() {
        let resp = SseStream::new(vec![
            SseEvent::data("event1"),
            SseEvent::data("event2"),
        ]).into_response();
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
}
