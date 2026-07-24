//! # kernway-htmx
//!
//! Typed access to the `HX-*` header vocabulary [htmx] uses, in both directions:
//! [`Htmx`] reads the request headers a client sends, [`HtmxResponse`] builds the
//! response headers the server sends back. No string header names in your code, a
//! typo is a compile error, and the classic `Vary: HX-Request` mistake is handled
//! for you.
//!
//! ## Supported htmx version
//!
//! **htmx 2.0.x** — the current major release, and what this crate is written and
//! tested against. The `HX-*` header vocabulary is **stable across htmx 1.9+ and
//! 2.0**, so a 1.9 client is served correctly too; what changed in 2.0 is
//! client-side (dropped legacy browsers, WebSocket/SSE moved to extensions) and
//! does not affect a server that speaks these headers. An `HX-*` header this crate
//! does not model is ignored, never rejected, so a newer htmx degrades to plain
//! HTML rather than a 400 — the forward-compatibility rule from the kernway-server
//! charter. The version is stated, not implied, per KEP-0000: a claim of "htmx
//! support" without a version is unverifiable.
//!
//! ## The one thing that is not a header
//!
//! htmx is a *client* library. A server does not "render htmx" — it renders HTML,
//! a full page or a fragment. What htmx asks of the server is exactly three
//! things, and this crate is exactly those three:
//!
//! 1. **Recognise the request** — [`Htmx::is_request`] and the other `HX-*`
//!    request accessors.
//! 2. **Return the right amount of HTML** — a fragment for an htmx request, a full
//!    page otherwise. [`Htmx::respond`] chooses, and sets `Vary: HX-Request` so a
//!    cache never serves a fragment to a browser expecting a page.
//! 3. **Speak the response vocabulary** — [`HtmxResponse`]: trigger events,
//!    redirect, retarget, reswap, push URLs.
//!
//! ## Security
//!
//! Every `HX-*` **request** header is attacker-controlled — a `curl` can send
//! `HX-Request: true`. Use them to decide *how to render*, never *whether to
//! allow*. Authorisation is a separate concern that does not read these headers.
//!
//! [htmx]: https://htmx.org

#![forbid(unsafe_code)]

use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::{IntoResponse, Response};

/// The htmx version this crate targets. The header vocabulary is stable from 1.9.
pub const HTMX_VERSION: &str = "2.0.x";

// ============================================================
// Request side — reading HX-* headers
// ============================================================

/// Typed access to the `HX-*` headers on an incoming request.
///
/// Borrows the request; every accessor is a header lookup, so it is cheap to
/// construct one per handler.
///
/// ```
/// use kernway_core::request::Request;
/// use kernway_htmx::Htmx;
///
/// let mut req = Request::new("GET", "/users");
/// req.headers.insert("hx-request", "true");
/// req.headers.insert("hx-target", "user-list");
///
/// let hx = Htmx::from(&req);
/// assert!(hx.is_request());
/// assert_eq!(hx.target(), Some("user-list"));
/// ```
#[derive(Clone, Copy)]
pub struct Htmx<'r> {
    req: &'r Request,
}

impl<'r> From<&'r Request> for Htmx<'r> {
    fn from(req: &'r Request) -> Self {
        Self { req }
    }
}

impl<'r> Htmx<'r> {
    /// Wrap a request. Same as [`Htmx::from`].
    #[must_use]
    pub fn new(req: &'r Request) -> Self {
        Self { req }
    }

    fn flag(&self, name: &str) -> bool {
        self.req.headers.get(name) == Some("true")
    }

    /// `HX-Request: true` — the request came from htmx rather than a normal
    /// navigation. The signal to return a fragment instead of a full page.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.flag("hx-request")
    }

    /// `HX-Boosted: true` — the request came from an `hx-boost`ed link or form.
    #[must_use]
    pub fn is_boosted(&self) -> bool {
        self.flag("hx-boosted")
    }

    /// `HX-History-Restore-Request: true` — htmx is restoring a history entry
    /// whose HTML was not in its cache, so a full page is wanted even though it
    /// is an htmx request.
    #[must_use]
    pub fn is_history_restore(&self) -> bool {
        self.flag("hx-history-restore-request")
    }

    /// `HX-Target` — the `id` of the target element.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.req.headers.get("hx-target")
    }

    /// `HX-Trigger` — the `id` of the element that triggered the request.
    #[must_use]
    pub fn trigger(&self) -> Option<&str> {
        self.req.headers.get("hx-trigger")
    }

    /// `HX-Trigger-Name` — the `name` of the triggering element.
    #[must_use]
    pub fn trigger_name(&self) -> Option<&str> {
        self.req.headers.get("hx-trigger-name")
    }

    /// `HX-Current-URL` — the browser's current URL when the request was made.
    #[must_use]
    pub fn current_url(&self) -> Option<&str> {
        self.req.headers.get("hx-current-url")
    }

    /// `HX-Prompt` — the user's response to an `hx-prompt`.
    #[must_use]
    pub fn prompt(&self) -> Option<&str> {
        self.req.headers.get("hx-prompt")
    }

    /// Serve a fragment to an htmx request, a full page otherwise — and mark the
    /// response `Vary: HX-Request`, so a shared cache never hands the fragment to
    /// a browser that asked for the page.
    ///
    /// This is the combinator that gets the caching right by construction: the
    /// classic htmx bug is one URL returning both shapes without a `Vary`, and a
    /// cache then serving a bare `<tbody>` into a blank page.
    ///
    /// ```
    /// # use kernway_core::request::Request;
    /// # use kernway_htmx::Htmx;
    /// # let mut req = Request::new("GET", "/users");
    /// # req.headers.insert("hx-request", "true");
    /// let hx = Htmx::from(&req);
    /// let resp = hx.respond(
    ///     || "<tr><td>a fragment</td></tr>".to_string(),
    ///     || "<html>the whole page</html>".to_string(),
    /// );
    /// ```
    pub fn respond(
        &self,
        fragment: impl FnOnce() -> String,
        full_page: impl FnOnce() -> String,
    ) -> HtmxResponse {
        let html = if self.is_request() && !self.is_history_restore() {
            fragment()
        } else {
            full_page()
        };
        HtmxResponse::new(html).vary_on_request()
    }
}

// ============================================================
// Response side — building HX-* headers
// ============================================================

/// How htmx should swap the response into the DOM (`HX-Reswap`, and the meaning
/// `hx-swap` carries). An enum, so a typo is a compile error rather than a swap
/// that silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// Replace the target's inner HTML.
    InnerHtml,
    /// Replace the target element itself.
    OuterHtml,
    /// Insert before the target.
    BeforeBegin,
    /// Insert as the target's first child.
    AfterBegin,
    /// Insert as the target's last child.
    BeforeEnd,
    /// Insert after the target.
    AfterEnd,
    /// Delete the target.
    Delete,
    /// Swap nothing.
    None,
}

impl Swap {
    /// The `hx-swap` / `HX-Reswap` token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Swap::InnerHtml => "innerHTML",
            Swap::OuterHtml => "outerHTML",
            Swap::BeforeBegin => "beforebegin",
            Swap::AfterBegin => "afterbegin",
            Swap::BeforeEnd => "beforeend",
            Swap::AfterEnd => "afterend",
            Swap::Delete => "delete",
            Swap::None => "none",
        }
    }
}

/// An HTML response plus the `HX-*` response headers htmx acts on.
///
/// Build the body, chain the directives, and it becomes a [`Response`] via
/// [`IntoResponse`]. All header names are set for you.
///
/// ```
/// use kernway_htmx::{HtmxResponse, Swap};
/// use kernway_core::response::IntoResponse;
///
/// let resp = HtmxResponse::new("<div>saved</div>")
///     .trigger("itemSaved")
///     .retarget("#status")
///     .reswap(Swap::InnerHtml)
///     .into_response();
/// ```
pub struct HtmxResponse {
    inner: Response,
    vary: bool,
}

impl HtmxResponse {
    /// A `200 OK` HTML response with the given body and no `HX-*` headers yet.
    pub fn new(html: impl Into<String>) -> Self {
        let inner = Response::new(StatusCode::OK)
            .content_type("text/html; charset=utf-8")
            .body(html.into().into_bytes());
        Self { inner, vary: false }
    }

    /// Wrap an existing response to attach `HX-*` headers to it.
    #[must_use]
    pub fn from_response(inner: Response) -> Self {
        Self { inner, vary: false }
    }

    fn set(mut self, name: &str, value: &str) -> Self {
        self.inner.headers.insert(name, value);
        self
    }

    /// Mark the response `Vary: HX-Request` — set this whenever the body differs
    /// between an htmx request and a normal one. [`Htmx::respond`] does it for you.
    #[must_use]
    pub fn vary_on_request(mut self) -> Self {
        self.vary = true;
        self
    }

    // --- client-side events ---

    /// `HX-Trigger` — fire a client-side event as soon as the response arrives.
    #[must_use]
    pub fn trigger(self, event: &str) -> Self {
        self.set("HX-Trigger", event)
    }

    /// `HX-Trigger-After-Settle` — fire the event after the settle step.
    #[must_use]
    pub fn trigger_after_settle(self, event: &str) -> Self {
        self.set("HX-Trigger-After-Settle", event)
    }

    /// `HX-Trigger-After-Swap` — fire the event after the swap.
    #[must_use]
    pub fn trigger_after_swap(self, event: &str) -> Self {
        self.set("HX-Trigger-After-Swap", event)
    }

    // --- navigation / history ---

    /// `HX-Redirect` — do a full client-side redirect to `url`.
    #[must_use]
    pub fn redirect(self, url: &str) -> Self {
        self.set("HX-Redirect", url)
    }

    /// `HX-Location` — navigate client-side to `url` without a full page reload.
    #[must_use]
    pub fn location(self, url: &str) -> Self {
        self.set("HX-Location", url)
    }

    /// `HX-Refresh: true` — tell the client to do a full page refresh.
    #[must_use]
    pub fn refresh(self) -> Self {
        self.set("HX-Refresh", "true")
    }

    /// `HX-Push-Url` — push `url` into the browser history.
    #[must_use]
    pub fn push_url(self, url: &str) -> Self {
        self.set("HX-Push-Url", url)
    }

    /// `HX-Replace-Url` — replace the current history entry with `url`.
    #[must_use]
    pub fn replace_url(self, url: &str) -> Self {
        self.set("HX-Replace-Url", url)
    }

    // --- targeting ---

    /// `HX-Retarget` — a CSS selector overriding which element is swapped.
    #[must_use]
    pub fn retarget(self, selector: &str) -> Self {
        self.set("HX-Retarget", selector)
    }

    /// `HX-Reswap` — override how the response is swapped in.
    #[must_use]
    pub fn reswap(self, swap: Swap) -> Self {
        self.set("HX-Reswap", swap.as_str())
    }

    /// `HX-Reselect` — a CSS selector choosing which part of the response to swap.
    #[must_use]
    pub fn reselect(self, selector: &str) -> Self {
        self.set("HX-Reselect", selector)
    }
}

impl IntoResponse for HtmxResponse {
    fn into_response(self) -> Response {
        let mut resp = self.inner;
        if self.vary {
            // Append to any existing Vary rather than clobbering it.
            match resp.headers.get("vary").map(str::to_string) {
                Some(existing) if !existing.to_ascii_lowercase().contains("hx-request") => {
                    resp.headers.insert("vary", &format!("{existing}, HX-Request"));
                }
                Some(_) => {} // already varies on HX-Request
                None => resp.headers.insert("vary", "HX-Request"),
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with(headers: &[(&str, &str)]) -> Request {
        let mut req = Request::new("GET", "/x");
        for (k, v) in headers {
            req.headers.insert(k, v);
        }
        req
    }

    // --- request side ---

    #[test]
    fn is_request_reads_the_flag() {
        assert!(Htmx::from(&req_with(&[("hx-request", "true")])).is_request());
        assert!(!Htmx::from(&req_with(&[])).is_request());
        // Any value other than "true" is not a request.
        assert!(!Htmx::from(&req_with(&[("hx-request", "false")])).is_request());
    }

    #[test]
    fn header_accessors() {
        let req = req_with(&[
            ("hx-target", "user-list"),
            ("hx-trigger", "save-btn"),
            ("hx-trigger-name", "save"),
            ("hx-current-url", "https://example.com/users"),
            ("hx-prompt", "yes"),
            ("hx-boosted", "true"),
        ]);
        let hx = Htmx::from(&req);
        assert_eq!(hx.target(), Some("user-list"));
        assert_eq!(hx.trigger(), Some("save-btn"));
        assert_eq!(hx.trigger_name(), Some("save"));
        assert_eq!(hx.current_url(), Some("https://example.com/users"));
        assert_eq!(hx.prompt(), Some("yes"));
        assert!(hx.is_boosted());
    }

    #[test]
    fn header_names_are_case_insensitive() {
        // Headers matches case-insensitively, so a client sending HX-Request works.
        assert!(Htmx::from(&req_with(&[("HX-Request", "true")])).is_request());
    }

    // --- respond combinator ---

    #[test]
    fn respond_serves_a_fragment_to_an_htmx_request_and_varies() {
        let req = req_with(&[("hx-request", "true")]);
        let resp = Htmx::from(&req)
            .respond(|| "FRAGMENT".to_string(), || "PAGE".to_string())
            .into_response();
        assert_eq!(resp.body_bytes(), b"FRAGMENT");
        assert_eq!(resp.headers.get("vary"), Some("HX-Request"));
    }

    #[test]
    fn respond_serves_the_full_page_to_a_normal_request() {
        let req = req_with(&[]);
        let resp = Htmx::from(&req)
            .respond(|| "FRAGMENT".to_string(), || "PAGE".to_string())
            .into_response();
        assert_eq!(resp.body_bytes(), b"PAGE");
        // Still varies — the same URL can return either shape.
        assert_eq!(resp.headers.get("vary"), Some("HX-Request"));
    }

    #[test]
    fn a_history_restore_gets_the_full_page_even_though_it_is_an_htmx_request() {
        let req = req_with(&[("hx-request", "true"), ("hx-history-restore-request", "true")]);
        let resp = Htmx::from(&req)
            .respond(|| "FRAGMENT".to_string(), || "PAGE".to_string())
            .into_response();
        assert_eq!(resp.body_bytes(), b"PAGE");
    }

    // --- response side ---

    #[test]
    fn response_sets_hx_headers() {
        let resp = HtmxResponse::new("<div/>")
            .trigger("saved")
            .retarget("#status")
            .reswap(Swap::InnerHtml)
            .push_url("/users/1")
            .into_response();
        assert_eq!(resp.headers.get("HX-Trigger"), Some("saved"));
        assert_eq!(resp.headers.get("HX-Retarget"), Some("#status"));
        assert_eq!(resp.headers.get("HX-Reswap"), Some("innerHTML"));
        assert_eq!(resp.headers.get("HX-Push-Url"), Some("/users/1"));
        assert_eq!(resp.headers.get("content-type"), Some("text/html; charset=utf-8"));
    }

    #[test]
    fn redirect_and_refresh() {
        assert_eq!(
            HtmxResponse::new("").redirect("/login").into_response().headers.get("HX-Redirect"),
            Some("/login")
        );
        assert_eq!(
            HtmxResponse::new("").refresh().into_response().headers.get("HX-Refresh"),
            Some("true")
        );
    }

    #[test]
    fn swap_tokens() {
        assert_eq!(Swap::OuterHtml.as_str(), "outerHTML");
        assert_eq!(Swap::AfterBegin.as_str(), "afterbegin");
        assert_eq!(Swap::Delete.as_str(), "delete");
    }

    #[test]
    fn vary_appends_rather_than_clobbers() {
        let base = Response::new(StatusCode::OK);
        let mut with_encoding = base;
        with_encoding.headers.insert("vary", "Accept-Encoding");
        let resp = HtmxResponse::from_response(with_encoding)
            .vary_on_request()
            .into_response();
        assert_eq!(resp.headers.get("vary"), Some("Accept-Encoding, HX-Request"));
    }
}
