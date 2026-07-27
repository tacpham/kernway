//! # kernleaf
//!
//! Kernway's template engine — the **Thymeleaf Standard Dialect**, in Rust.
//! Templates are HTML with `th:*` attributes, so a `.html` file is valid on its
//! own and opens in a browser showing its placeholder content; the engine
//! *overrides* that content at render time. This is Thymeleaf's defining feature,
//! **natural templates**: a designer previews the page without running the server.
//!
//! A template is parsed **once** into a DOM the engine caches ([`Kernleaf::add`]),
//! and rendered against the [`Value`] model ([KEP-0003]) by walking that DOM —
//! parsing never happens on the request path
//! ([KEP-0000 §4](https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md)).
//! It is **HTML-safe by default**: `th:text` escapes, and only the explicit
//! `th:utext` emits raw HTML.
//!
//! ## Attributes (this slice, A)
//!
//! ```html
//! <h1 th:text="${title}">Placeholder title</h1>       <!-- escaped text -->
//! <div th:utext="${trustedHtml}">…</div>              <!-- raw, explicit -->
//! <div th:if="${user.admin}">Admin</div>              <!-- conditional -->
//! <div th:unless="${user.admin}">User</div>
//! <li th:each="p : ${posts}" th:text="${p.title}">Title</li>
//! <a th:href="${url}" href="/fallback">Link</a>       <!-- th:<attr> sets that attribute -->
//! ```
//!
//! Expressions in slice A are variable/property paths (`${user.name}`) and string
//! literals (`'text'`). Operators, `@{...}` URLs, `#{...}` messages, and utility
//! objects are later slices — see the module charter.
//!
//! ## Example
//!
//! ```
//! use kernleaf::Kernleaf;
//! use kernway_core::template::{TemplateEngine, Value};
//!
//! let mut engine = Kernleaf::new();
//! engine.add("greeting", "<h1 th:text=\"${name}\">Name</h1>").unwrap();
//!
//! let model = Value::map([("name", Value::from("Alice"))]);
//! assert_eq!(engine.render("greeting", &model).unwrap(), "<h1>Alice</h1>");
//! ```
//!
//! [KEP-0003]: https://github.com/tacpham/kernway/blob/main/docs/kep/0003-template-model.md
//! [`Value`]: kernway_core::template::Value

#![forbid(unsafe_code)]

use std::collections::HashMap;

use kernway_core::security::{Anonymous, Authorization};
use kernway_core::template::{TemplateEngine, TemplateError, Value};

mod expr;
use expr::{Env, Expr};

/// The fail-closed default authorization: anonymous, no roles.
static ANONYMOUS: Anonymous = Anonymous;

/// The hidden form field the CSRF token is injected under — matches
/// `kernway_security::csrf::FIELD` by convention (kept a plain const so the
/// template engine does not depend on the security crate).
const CSRF_FIELD: &str = "_csrf";

/// Per-render context beyond the model: the authorization facts `th:authorize`
/// checks, and the CSRF token auto-injected into state-changing forms. Both
/// default to "none" — `th:authorize` then denies (fail-closed) and no CSRF field
/// is added.
#[derive(Default)]
pub struct RenderContext<'a> {
    authz: Option<&'a dyn Authorization>,
    csrf: Option<&'a str>,
}

impl<'a> RenderContext<'a> {
    /// An empty context — anonymous, no CSRF token.
    pub fn new() -> Self {
        Self {
            authz: None,
            csrf: None,
        }
    }

    /// Supply the authorization facts `th:authorize` checks.
    #[must_use]
    pub fn authorize(mut self, authz: &'a dyn Authorization) -> Self {
        self.authz = Some(authz);
        self
    }

    /// Supply the CSRF token to inject into state-changing forms.
    #[must_use]
    pub fn csrf(mut self, token: &'a str) -> Self {
        self.csrf = Some(token);
        self
    }
}

// ============================================================
// IR — the parsed template DOM, cached and walked at render time
// ============================================================

/// Every template's compiled DOM, keyed by name — searched to resolve a
/// `th:insert`/`th:replace` fragment reference.
type Templates = HashMap<String, Vec<Dom>>;

/// The deepest a fragment may nest, a backstop against a fragment that includes
/// itself.
const MAX_FRAGMENT_DEPTH: u16 = 32;

/// A node in the parsed template. This *is* the cached IR: `add` parses to it
/// once, `render` walks it.
// `Element` is the large variant by design — boxing it would add a pointer chase
// to every element on the render hot path to shrink the rarer text nodes, the
// wrong trade for an IR that is walked far more often than it is built.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum Dom {
    /// Literal text with no inline expressions — emitted verbatim. The common
    /// case, and a straight `push_str` at render time.
    Text(String),
    /// Text containing `[[${…}]]` / `[(${…})]` inline expressions, pre-split at
    /// parse time so the request path never scans a plain text node.
    Inline(Vec<InlinePart>),
    /// `<!-- … -->`, kept so the output stays a faithful copy.
    Comment(String),
    /// A doctype or other `<!…>` declaration, emitted verbatim.
    Declaration(String),
    /// An element, with its `th:*` directives already extracted.
    Element(Element),
}

/// One piece of an inlined text node.
#[derive(Debug, Clone)]
enum InlinePart {
    /// Literal text between inline expressions.
    Lit(String),
    /// `[[${…}]]` — escaped per the current inline mode (HTML / JS / CSS).
    Escaped(Expr),
    /// `[(${…})]` — emitted raw, the explicit unescaped inline form.
    Raw(Expr),
}

/// The escaping context for inline `[[…]]` expressions, set by `th:inline` and
/// inherited by descendants. A `Copy` enum threaded through render — the whole
/// mechanism for context-aware escaping without a per-character state machine on
/// the HTML path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum InlineMode {
    /// Default: HTML-escape (same rule as `th:text`).
    Html,
    /// `th:inline="javascript"` — escape for a JS string/script context.
    JavaScript,
    /// `th:inline="css"` — escape for a CSS context.
    Css,
}

/// A parsed element with its Thymeleaf directives pulled out of the raw attrs.
#[derive(Debug, Clone)]
struct Element {
    tag: String,
    /// Whether the tag is void (`<br>`, `<img>`) — no children, no closing tag.
    void: bool,
    /// Ordinary attributes to emit as-is (`th:*` and `xmlns:th` removed).
    static_attrs: Vec<(String, String)>,
    /// `th:<name>` attributes that set an attribute from an expression
    /// (`th:href` → `href`).
    dynamic_attrs: Vec<(String, Expr)>,
    th_if: Option<Expr>,
    th_unless: Option<Expr>,
    th_each: Option<(String, Expr)>,
    th_text: Option<Expr>,
    th_utext: Option<Expr>,
    /// `th:inline="javascript|css|text|none"` — the escape mode for `[[…]]` in
    /// this element's subtree; `None` inherits the enclosing mode.
    inline_mode: Option<InlineMode>,
    /// `th:authorize="…"` — drop the element unless the check passes.
    th_authorize: Option<AuthzExpr>,
    /// `th:fragment="name"` — marks this element as a named, reusable fragment.
    th_fragment: Option<String>,
    /// `th:insert="~{tpl :: name}"` — render the fragment *inside* this element.
    th_insert: Option<FragmentRef>,
    /// `th:replace="~{tpl :: name}"` — replace this element *with* the fragment.
    th_replace: Option<FragmentRef>,
    children: Vec<Dom>,
}

/// A reference to a fragment: `tpl :: name`, `:: name` (this template), or `tpl`
/// (a whole template). Parameterised fragments are a later addition.
#[derive(Debug, Clone)]
struct FragmentRef {
    /// The template to look in; `None` means "any loaded template".
    template: Option<String>,
    /// The `th:fragment` name; empty means "the whole template".
    name: String,
}

/// A parsed `th:authorize` security check — the Spring-Security expression subset.
#[derive(Debug, Clone)]
enum AuthzExpr {
    /// `permitAll` — always render.
    PermitAll,
    /// `denyAll` — never render.
    DenyAll,
    /// `isAuthenticated()`.
    IsAuthenticated,
    /// `isAnonymous()`.
    IsAnonymous,
    /// `hasRole('X')`.
    HasRole(String),
    /// `hasAnyRole('A','B')` — true if the principal has any listed role.
    HasAnyRole(Vec<String>),
}

impl AuthzExpr {
    fn allows(&self, authz: &dyn kernway_core::security::Authorization) -> bool {
        match self {
            AuthzExpr::PermitAll => true,
            AuthzExpr::DenyAll => false,
            AuthzExpr::IsAuthenticated => authz.is_authenticated(),
            AuthzExpr::IsAnonymous => !authz.is_authenticated(),
            AuthzExpr::HasRole(r) => authz.has_role(r),
            AuthzExpr::HasAnyRole(rs) => rs.iter().any(|r| authz.has_role(r)),
        }
    }
}

/// The template engine — every template's parsed DOM, keyed by name, plus the
/// i18n message bundle that `#{...}` resolves against.
#[derive(Default)]
pub struct Kernleaf {
    templates: Templates,
    messages: HashMap<String, String>,
}

impl Kernleaf {
    /// A new engine with no templates and no messages.
    pub fn new() -> Self {
        Self {
            templates: HashMap::new(),
            messages: HashMap::new(),
        }
    }

    /// Register an i18n message. `#{key}` resolves to `value`, with `{0}`/`{1}`
    /// placeholders filled from the arguments in `#{key(arg0, arg1)}`.
    pub fn message(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.messages.insert(key.into(), value.into());
    }

    /// Parse `source` under `name`. Parsing happens **here**, once — a malformed
    /// template is reported now, not on the first request. Re-adding replaces
    /// (what the M5 hot-reload watcher calls on a file change).
    pub fn add(&mut self, name: impl Into<String>, source: &str) -> Result<(), TemplateError> {
        let dom = Parser::new(source).parse_nodes()?;
        self.templates.insert(name.into(), dom);
        Ok(())
    }

    /// Whether a template of this name is compiled and ready.
    pub fn is_compiled(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }
}

impl Kernleaf {
    /// Render with a [`RenderContext`] — the form that supplies `th:authorize`
    /// facts and a CSRF token.
    pub fn render_with(
        &self,
        template: &str,
        model: &Value<'_>,
        ctx: &RenderContext<'_>,
    ) -> Result<String, TemplateError> {
        let dom = self
            .templates
            .get(template)
            .ok_or_else(|| err(format!("no template named `{template}`")))?;
        let env = Env {
            model,
            messages: &self.messages,
            context_path: "",
            authz: ctx.authz.unwrap_or(&ANONYMOUS),
            csrf: ctx.csrf,
        };
        let mut out = String::new();
        let mut scope: Vec<(&str, &Value<'_>)> = Vec::new();
        render_nodes(
            dom,
            &env,
            &mut scope,
            InlineMode::Html,
            &self.templates,
            0,
            &mut out,
        )?;
        Ok(out)
    }
}

impl TemplateEngine for Kernleaf {
    fn render(&self, template: &str, model: &Value<'_>) -> Result<String, TemplateError> {
        self.render_with(template, model, &RenderContext::new())
    }
}

// ============================================================
// Rendering — walk the DOM against the model
// ============================================================

#[allow(clippy::too_many_arguments)]
fn render_nodes<'ir, 'm>(
    nodes: &'ir [Dom],
    env: &Env<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    mode: InlineMode,
    templates: &'ir Templates,
    depth: u16,
    out: &mut String,
) -> Result<(), TemplateError> {
    for node in nodes {
        match node {
            Dom::Text(t) => out.push_str(t),
            Dom::Inline(parts) => render_inline(parts, env, scope, mode, out),
            Dom::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            Dom::Declaration(d) => out.push_str(d),
            Dom::Element(el) => render_element(el, env, scope, mode, templates, depth, out)?,
        }
    }
    Ok(())
}

/// Render a pre-split inline text node, escaping `[[…]]` parts per `mode`.
fn render_inline<'m>(
    parts: &[InlinePart],
    env: &Env<'m>,
    scope: &[(&str, &'m Value<'m>)],
    mode: InlineMode,
    out: &mut String,
) {
    for part in parts {
        match part {
            InlinePart::Lit(s) => out.push_str(s),
            InlinePart::Raw(e) => expr::eval(e, env, scope).write_string(out),
            InlinePart::Escaped(e) => {
                let mut buf = String::new();
                expr::eval(e, env, scope).write_string(&mut buf);
                escape_for(mode, &buf, out);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_element<'ir, 'm>(
    el: &'ir Element,
    env: &Env<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    mode: InlineMode,
    templates: &'ir Templates,
    depth: u16,
    out: &mut String,
) -> Result<(), TemplateError> {
    // Precedence matches Thymeleaf: th:each (outer) wraps th:if (inner).
    if let Some((var, seq)) = &el.th_each {
        // A non-sequence (missing, or a scalar) is zero iterations — lenient.
        if let Some(items) = expr::eval(seq, env, scope).as_seq() {
            for item in items {
                scope.push((var.as_str(), item));
                let r = render_instance(el, env, scope, mode, templates, depth, out);
                scope.pop();
                r?;
            }
        }
        Ok(())
    } else {
        render_instance(el, env, scope, mode, templates, depth, out)
    }
}

/// Render one element instance (th:each already resolved for this iteration):
/// evaluate th:if/th:unless, then open tag + attributes + body + close.
#[allow(clippy::too_many_arguments)]
fn render_instance<'ir, 'm>(
    el: &'ir Element,
    env: &Env<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    mode: InlineMode,
    templates: &'ir Templates,
    depth: u16,
    out: &mut String,
) -> Result<(), TemplateError> {
    // th:authorize has the highest precedence: an unauthorized element (and its
    // whole subtree) is never rendered. Fail-closed — no context denies.
    if let Some(auth) = &el.th_authorize {
        if !auth.allows(env.authz) {
            return Ok(());
        }
    }
    if let Some(cond) = &el.th_if {
        if !expr::eval(cond, env, scope).to_bool() {
            return Ok(());
        }
    }
    if let Some(cond) = &el.th_unless {
        if expr::eval(cond, env, scope).to_bool() {
            return Ok(());
        }
    }

    // th:replace — the fragment takes this element's place entirely (its own tag
    // and all). Nothing of the host is emitted.
    if let Some(fref) = &el.th_replace {
        if depth >= MAX_FRAGMENT_DEPTH {
            return Err(err("fragment nesting too deep — a fragment cycle?"));
        }
        if let Some(frag) = resolve_fragment(templates, fref) {
            render_nodes(frag, env, scope, mode, templates, depth + 1, out)?;
        }
        return Ok(());
    }

    out.push('<');
    out.push_str(&el.tag);
    for (name, value) in &el.static_attrs {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        escape_html_into(value, out);
        out.push('"');
    }
    // th:<attr> — evaluate and emit, escaped as an attribute value. A dynamic
    // attribute of the same name as a static one overrides it (last write wins);
    // slice A keeps it simple and just appends — templates rarely set both.
    for (name, e) in &el.dynamic_attrs {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        let mut buf = String::new();
        expr::eval(e, env, scope).write_string(&mut buf);
        escape_html_into(&buf, out);
        out.push('"');
    }

    if el.void {
        out.push('>');
        return Ok(());
    }
    out.push('>');

    // Auto-CSRF: a state-changing <form> gets a hidden `_csrf` field injected as
    // its first child, so a developer cannot forget it. The `env.csrf` check
    // comes first so that when no token is in play — the common case — a plain
    // element never pays for the `form` tag comparison.
    if let Some(token) = env.csrf {
        if el.tag.eq_ignore_ascii_case("form") && is_state_changing_form(el) {
            out.push_str("<input type=\"hidden\" name=\"");
            out.push_str(CSRF_FIELD);
            out.push_str("\" value=\"");
            escape_html_into(token, out);
            out.push_str("\">");
        }
    }

    // Body, in precedence order: th:insert (fragment inside), th:text/th:utext
    // (expression), otherwise the children (the natural-template placeholder).
    let child_mode = el.inline_mode.unwrap_or(mode);
    if let Some(fref) = &el.th_insert {
        if depth >= MAX_FRAGMENT_DEPTH {
            return Err(err("fragment nesting too deep — a fragment cycle?"));
        }
        if let Some(frag) = resolve_fragment(templates, fref) {
            render_nodes(frag, env, scope, child_mode, templates, depth + 1, out)?;
        }
    } else if let Some(e) = &el.th_text {
        let mut buf = String::new();
        expr::eval(e, env, scope).write_string(&mut buf);
        escape_html_into(&buf, out);
    } else if let Some(e) = &el.th_utext {
        expr::eval(e, env, scope).write_string(out); // raw — the explicit unsafe path
    } else {
        render_nodes(&el.children, env, scope, child_mode, templates, depth, out)?;
    }

    out.push_str("</");
    out.push_str(&el.tag);
    out.push('>');
    Ok(())
}

/// Escape `s` for the given inline context — the context-aware escaping the KEP
/// calls for. HTML is the default; JS and CSS have their own rules, selected once
/// per `th:inline` block, never a per-character state machine on the HTML path.
fn escape_for(mode: InlineMode, s: &str, out: &mut String) {
    match mode {
        InlineMode::Html => escape_html_into(s, out),
        InlineMode::JavaScript => escape_js_into(s, out),
        InlineMode::Css => escape_css_into(s, out),
    }
}

/// HTML-escape into `out` — the five characters that matter in element content
/// and double-quoted attributes.
fn escape_html_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
}

/// Escape a value to sit inside a JS string literal in a `<script>` — the JS
/// context. Quotes, backslash, and line terminators are backslash-escaped; the
/// `<`/`>`/`&`/`/` that could close the script or start a tag become `\uXXXX`, so
/// a value can never break out of the script element or the string.
fn escape_js_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '/' => out.push_str("\\/"),
            _ => out.push(c),
        }
    }
}

/// Escape a value for a CSS context — anything outside `[A-Za-z0-9]` is
/// backslash-hex-escaped, which is safe in a CSS string or identifier.
fn escape_css_into(s: &str, out: &mut String) {
    use std::fmt::Write as _;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            let _ = write!(out, "\\{:X} ", c as u32);
        }
    }
}

// ============================================================
// Parsing — HTML with th:* attributes → DOM
// ============================================================

/// HTML void elements: no closing tag, no children.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            s: src.as_bytes(),
            src,
            pos: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.s.len()
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.src[self.pos..].starts_with(pat)
    }

    /// Parse a run of sibling nodes, stopping (without consuming) at a closing
    /// tag that belongs to an ancestor, or at EOF.
    fn parse_nodes(&mut self) -> Result<Vec<Dom>, TemplateError> {
        let mut nodes = Vec::new();
        while !self.eof() {
            if self.starts_with("</") {
                break; // a close tag — the caller (or EOF handling) owns it
            }
            if self.s[self.pos] == b'<' {
                if self.starts_with("<!--") {
                    nodes.push(self.read_comment()?);
                } else if self.starts_with("<!") {
                    nodes.push(self.read_declaration()?);
                } else {
                    nodes.push(self.read_element()?);
                }
            } else {
                nodes.push(self.read_text()?);
            }
        }
        Ok(nodes)
    }

    fn read_text(&mut self) -> Result<Dom, TemplateError> {
        let start = self.pos;
        while !self.eof() && self.s[self.pos] != b'<' {
            self.pos += 1;
        }
        // The inline scan happens here, at parse time — a plain text node stays
        // `Dom::Text` and pays nothing at render.
        compile_text(&self.src[start..self.pos])
    }

    fn read_comment(&mut self) -> Result<Dom, TemplateError> {
        self.pos += 4; // <!--
        let start = self.pos;
        let end = self.src[start..]
            .find("-->")
            .ok_or_else(|| err("unterminated comment"))?;
        let body = self.src[start..start + end].to_string();
        self.pos = start + end + 3;
        Ok(Dom::Comment(body))
    }

    fn read_declaration(&mut self) -> Result<Dom, TemplateError> {
        let start = self.pos;
        let end = self.src[start..]
            .find('>')
            .ok_or_else(|| err("unterminated `<!…>`"))?;
        let text = self.src[start..start + end + 1].to_string();
        self.pos = start + end + 1;
        Ok(Dom::Declaration(text))
    }

    fn read_element(&mut self) -> Result<Dom, TemplateError> {
        self.pos += 1; // consume '<'
        let tag = self.read_name();
        if tag.is_empty() {
            return Err(err("`<` without a tag name"));
        }
        let tag_lower = tag.to_ascii_lowercase();

        let (raw_attrs, self_closing) = self.read_attrs()?;
        let void = self_closing || VOID.contains(&tag_lower.as_str());

        let children = if void {
            Vec::new()
        } else {
            self.parse_nodes()?
        };
        if !void {
            // Consume the matching close tag if present. Lenient: a missing or
            // mismatched close tag does not abort — templates in the wild have
            // both, and a hard error here helps no one.
            if self.starts_with("</") {
                self.pos += 2;
                let _ = self.read_name();
                self.skip_ws();
                if !self.eof() && self.s[self.pos] == b'>' {
                    self.pos += 1;
                }
            }
        }

        Ok(Dom::Element(build_element(tag, void, raw_attrs, children)?))
    }

    fn read_name(&mut self) -> String {
        let start = self.pos;
        while !self.eof() {
            let c = self.s[self.pos];
            if c.is_ascii_whitespace() || c == b'>' || c == b'/' || c == b'=' {
                break;
            }
            self.pos += 1;
        }
        self.src[start..self.pos].to_string()
    }

    fn skip_ws(&mut self) {
        while !self.eof() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    /// Read attributes up to `>` or `/>`. Returns the pairs and whether it was a
    /// self-closing tag.
    fn read_attrs(&mut self) -> Result<(Vec<(String, String)>, bool), TemplateError> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            if self.eof() {
                return Err(err("unterminated tag"));
            }
            if self.s[self.pos] == b'>' {
                self.pos += 1;
                return Ok((attrs, false));
            }
            if self.starts_with("/>") {
                self.pos += 2;
                return Ok((attrs, true));
            }
            let name = self.read_name();
            if name.is_empty() {
                // A stray character we do not recognise as a name; skip it so the
                // parser cannot get stuck.
                self.pos += 1;
                continue;
            }
            self.skip_ws();
            let value = if !self.eof() && self.s[self.pos] == b'=' {
                self.pos += 1;
                self.skip_ws();
                self.read_attr_value()
            } else {
                String::new() // a valueless attribute (e.g. `disabled`)
            };
            attrs.push((name, value));
        }
    }

    fn read_attr_value(&mut self) -> String {
        if self.eof() {
            return String::new();
        }
        let quote = self.s[self.pos];
        if quote == b'"' || quote == b'\'' {
            self.pos += 1;
            let start = self.pos;
            while !self.eof() && self.s[self.pos] != quote {
                self.pos += 1;
            }
            let value = self.src[start..self.pos].to_string();
            if !self.eof() {
                self.pos += 1; // closing quote
            }
            value
        } else {
            // Unquoted: read until whitespace or tag end.
            let start = self.pos;
            while !self.eof() {
                let c = self.s[self.pos];
                if c.is_ascii_whitespace() || c == b'>' || c == b'/' {
                    break;
                }
                self.pos += 1;
            }
            self.src[start..self.pos].to_string()
        }
    }
}

/// Split a text run into inline parts. Plain text (no `[[…]]`/`[(…)]`) returns
/// `Dom::Text` — the fast path, so the request never re-scans it. This is the
/// discipline that keeps context-aware escaping off the common path: the scan is
/// a one-time parse cost.
fn compile_text(raw: &str) -> Result<Dom, TemplateError> {
    if !raw.contains("[[") && !raw.contains("[(") {
        return Ok(Dom::Text(raw.to_string()));
    }
    let mut parts = Vec::new();
    let mut rest = raw;
    loop {
        let escaped = rest.find("[[");
        let unescaped = rest.find("[(");
        let (pos, is_escaped) = match (escaped, unescaped) {
            (Some(a), Some(b)) => {
                if a < b {
                    (a, true)
                } else {
                    (b, false)
                }
            }
            (Some(a), None) => (a, true),
            (None, Some(b)) => (b, false),
            (None, None) => {
                if !rest.is_empty() {
                    parts.push(InlinePart::Lit(rest.to_string()));
                }
                break;
            }
        };
        if pos > 0 {
            parts.push(InlinePart::Lit(rest[..pos].to_string()));
        }
        let after = &rest[pos + 2..];
        let close = if is_escaped { "]]" } else { ")]" };
        let end = after.find(close).ok_or_else(|| {
            err(format!(
                "unclosed inline `{}`",
                if is_escaped { "[[" } else { "[(" }
            ))
        })?;
        let expr = parse_expr(after[..end].trim())?;
        parts.push(if is_escaped {
            InlinePart::Escaped(expr)
        } else {
            InlinePart::Raw(expr)
        });
        rest = &after[end + 2..];
    }
    Ok(Dom::Inline(parts))
}

/// Split an element's raw attributes into static ones and the `th:*` directives.
fn build_element(
    tag: String,
    void: bool,
    raw_attrs: Vec<(String, String)>,
    children: Vec<Dom>,
) -> Result<Element, TemplateError> {
    let mut el = Element {
        tag,
        void,
        static_attrs: Vec::new(),
        dynamic_attrs: Vec::new(),
        th_if: None,
        th_unless: None,
        th_each: None,
        th_text: None,
        th_utext: None,
        inline_mode: None,
        th_authorize: None,
        th_fragment: None,
        th_insert: None,
        th_replace: None,
        children,
    };

    for (name, value) in raw_attrs {
        // Drop the namespace declaration; it exists only to make the raw file
        // valid, and Thymeleaf removes it from the output too.
        if name == "xmlns:th" {
            continue;
        }
        match name.strip_prefix("th:") {
            None => el.static_attrs.push((name, value)),
            Some(directive) => match directive {
                "text" => el.th_text = Some(parse_expr(&value)?),
                "utext" => el.th_utext = Some(parse_expr(&value)?),
                "if" => el.th_if = Some(parse_expr(&value)?),
                "unless" => el.th_unless = Some(parse_expr(&value)?),
                "each" => el.th_each = Some(parse_each(&value)?),
                "inline" => {
                    el.inline_mode = Some(match value.trim().trim_matches('\'') {
                        "javascript" | "js" => InlineMode::JavaScript,
                        "css" => InlineMode::Css,
                        _ => InlineMode::Html, // "text", "none", or anything else
                    })
                }
                "authorize" => el.th_authorize = Some(parse_authorize(&value)?),
                "fragment" => {
                    // Drop any `(params)` — parameterised fragments are a later slice.
                    let name = value.split('(').next().unwrap_or("").trim().to_string();
                    el.th_fragment = Some(name);
                }
                "insert" => el.th_insert = Some(parse_fragment_ref(&value)),
                "replace" => el.th_replace = Some(parse_fragment_ref(&value)),
                attr => el
                    .dynamic_attrs
                    .push((attr.to_string(), parse_expr(&value)?)),
            },
        }
    }
    Ok(el)
}

/// Parse a fragment reference: `~{tpl :: name}`, `tpl :: name`, `:: name`
/// (this-template), or `tpl` (whole template). `(params)` on the name are dropped
/// for now (parameterised fragments are a later slice).
fn parse_fragment_ref(value: &str) -> FragmentRef {
    let v = value.trim();
    // Strip an optional `~{ … }` wrapper.
    let v = v
        .strip_prefix("~{")
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(v)
        .trim();
    let (template, name) = match v.split_once("::") {
        Some((tpl, name)) => {
            let tpl = tpl.trim();
            (
                if tpl.is_empty() {
                    None
                } else {
                    Some(tpl.to_string())
                },
                name.trim(),
            )
        }
        None => (Some(v.to_string()), ""), // a bare template name = the whole template
    };
    let name = name.split('(').next().unwrap_or("").trim().to_string();
    FragmentRef { template, name }
}

/// Find a fragment's nodes for a reference. A named fragment resolves to the
/// single element carrying `th:fragment="name"`; an empty name resolves to the
/// whole template's nodes.
fn resolve_fragment<'ir>(templates: &'ir Templates, frag: &FragmentRef) -> Option<&'ir [Dom]> {
    // Which templates to search: the named one, or all of them.
    let search: Vec<&'ir Vec<Dom>> = match &frag.template {
        Some(tpl) => templates.get(tpl).into_iter().collect(),
        None => templates.values().collect(),
    };
    if frag.name.is_empty() {
        // A whole-template reference.
        return search.into_iter().next().map(Vec::as_slice);
    }
    for nodes in search {
        if let Some(node) = find_fragment(nodes, &frag.name) {
            return Some(std::slice::from_ref(node));
        }
    }
    None
}

/// The `Dom::Element` carrying `th:fragment="name"`, searched depth-first.
fn find_fragment<'ir>(nodes: &'ir [Dom], name: &str) -> Option<&'ir Dom> {
    for node in nodes {
        if let Dom::Element(el) = node {
            if el.th_fragment.as_deref() == Some(name) {
                return Some(node);
            }
            if let Some(found) = find_fragment(&el.children, name) {
                return Some(found);
            }
        }
    }
    None
}

/// Whether a `<form>` uses a state-changing method (anything but GET/absent),
/// so it needs a CSRF token. The method may be a static attribute (`method="post"`)
/// or already been rendered via `th:method`; a `_csrf` field is only useful for
/// the methods a CSRF check guards.
fn is_state_changing_form(el: &Element) -> bool {
    el.static_attrs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("method"))
        .map(|(_, v)| !v.eq_ignore_ascii_case("get"))
        .unwrap_or(false)
}

/// Parse a `th:authorize` security expression: `permitAll`, `denyAll`,
/// `isAuthenticated()`, `isAnonymous()`, `hasRole('X')`, `hasAnyRole('A','B')`.
fn parse_authorize(value: &str) -> Result<AuthzExpr, TemplateError> {
    let v = value.trim().trim_end_matches("()").trim();
    match v {
        "permitAll" => return Ok(AuthzExpr::PermitAll),
        "denyAll" => return Ok(AuthzExpr::DenyAll),
        "isAuthenticated" => return Ok(AuthzExpr::IsAuthenticated),
        "isAnonymous" => return Ok(AuthzExpr::IsAnonymous),
        _ => {}
    }
    if let Some(args) = fn_call(value.trim(), "hasRole") {
        let role = args.trim().trim_matches('\'').trim_matches('"');
        return Ok(AuthzExpr::HasRole(role.to_string()));
    }
    if let Some(args) = fn_call(value.trim(), "hasAnyRole") {
        let roles = args
            .split(',')
            .map(|r| r.trim().trim_matches('\'').trim_matches('"').to_string())
            .filter(|r| !r.is_empty())
            .collect();
        return Ok(AuthzExpr::HasAnyRole(roles));
    }
    Err(err(format!(
        "unsupported th:authorize expression `{value}`"
    )))
}

/// If `s` is `name(args)`, return `args` (without the parens).
fn fn_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// `item : ${items}` → (`"item"`, `Expr::Var(["items"])`).
fn parse_each(value: &str) -> Result<(String, Expr), TemplateError> {
    let (var, seq) = value
        .split_once(':')
        .ok_or_else(|| err(format!("th:each needs `var : ${{seq}}`: `{value}`")))?;
    let var = var.trim();
    // An iteration-status variable (`item, stat : …`) is a later slice; take the
    // first name for now.
    let var = var.split(',').next().unwrap_or("").trim();
    if var.is_empty() {
        return Err(err(format!("th:each has no loop variable: `{value}`")));
    }
    Ok((var.to_string(), parse_expr(seq.trim())?))
}

/// Parse an attribute value into an expression — the full Standard Expression
/// grammar (variables, literals, operators, ternary/elvis, `|…|`). See [`expr`].
fn parse_expr(value: &str) -> Result<Expr, TemplateError> {
    expr::parse(value).map_err(err)
}

fn err(msg: impl Into<String>) -> TemplateError {
    TemplateError(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, model: &Value<'_>) -> Result<String, TemplateError> {
        let mut engine = Kernleaf::new();
        engine.add("t", source)?;
        engine.render("t", model)
    }

    // --- natural templates: no th: passes through verbatim ----------------

    #[test]
    fn plain_html_is_unchanged() {
        let html = "<div class=\"card\"><p>Hello</p><br>done</div>";
        assert_eq!(render(html, &Value::Null).unwrap(), html);
    }

    #[test]
    fn a_doctype_and_comment_survive() {
        let html = "<!DOCTYPE html><!-- note --><p>hi</p>";
        assert_eq!(render(html, &Value::Null).unwrap(), html);
    }

    // --- th:text — escaped by default (the natural-template override) ------

    #[test]
    fn th_text_replaces_body_with_escaped_value() {
        let m = Value::map([("title", Value::from("Team"))]);
        assert_eq!(
            render("<h1 th:text=\"${title}\">Placeholder</h1>", &m).unwrap(),
            "<h1>Team</h1>"
        );
    }

    #[test]
    fn th_text_escapes_html_the_xss_gate() {
        let m = Value::map([("q", Value::from("<script>alert(1)</script>"))]);
        let out = render("<p th:text=\"${q}\">x</p>", &m).unwrap();
        assert_eq!(out, "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>");
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn th_utext_is_raw_the_explicit_unsafe_path() {
        let m = Value::map([("html", Value::from("<b>bold</b>"))]);
        assert_eq!(
            render("<div th:utext=\"${html}\">x</div>", &m).unwrap(),
            "<div><b>bold</b></div>"
        );
    }

    #[test]
    fn a_string_literal_expression() {
        assert_eq!(
            render("<p th:text=\"'hi'\">x</p>", &Value::Null).unwrap(),
            "<p>hi</p>"
        );
    }

    // --- th:if / th:unless -------------------------------------------------

    #[test]
    fn th_if_includes_or_drops_the_element() {
        let t = "<div th:if=\"${admin}\">panel</div>";
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(true))])).unwrap(),
            "<div>panel</div>"
        );
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(false))])).unwrap(),
            ""
        );
    }

    #[test]
    fn th_unless_is_the_inverse() {
        let t = "<div th:unless=\"${admin}\">hi</div>";
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(false))])).unwrap(),
            "<div>hi</div>"
        );
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(true))])).unwrap(),
            ""
        );
    }

    // --- th:each -----------------------------------------------------------

    #[test]
    fn th_each_repeats_the_element() {
        let m = Value::map([(
            "posts",
            Value::seq([
                Value::map([("title", Value::from("A"))]),
                Value::map([("title", Value::from("B"))]),
            ]),
        )]);
        let out = render(
            "<ul><li th:each=\"p : ${posts}\" th:text=\"${p.title}\">t</li></ul>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<ul><li>A</li><li>B</li></ul>");
    }

    #[test]
    fn th_each_over_a_missing_sequence_is_empty() {
        assert_eq!(
            render("<ul><li th:each=\"p : ${posts}\">x</li></ul>", &Value::Null).unwrap(),
            "<ul></ul>"
        );
    }

    #[test]
    fn th_each_wraps_th_if_per_item() {
        // Precedence: each first, then if — so this filters within the loop.
        let m = Value::map([(
            "xs",
            Value::seq([
                Value::map([("ok", Value::from(true)), ("n", Value::from(1))]),
                Value::map([("ok", Value::from(false)), ("n", Value::from(2))]),
                Value::map([("ok", Value::from(true)), ("n", Value::from(3))]),
            ]),
        )]);
        let t = "<li th:each=\"x : ${xs}\" th:if=\"${x.ok}\" th:text=\"${x.n}\">n</li>";
        assert_eq!(render(t, &m).unwrap(), "<li>1</li><li>3</li>");
    }

    // --- th:<attr> ---------------------------------------------------------

    #[test]
    fn th_attr_sets_an_attribute_from_an_expression() {
        let m = Value::map([("url", Value::from("/users/1"))]);
        assert_eq!(
            render("<a th:href=\"${url}\" href=\"/fallback\">go</a>", &m).unwrap(),
            "<a href=\"/fallback\" href=\"/users/1\">go</a>"
        );
    }

    #[test]
    fn attribute_values_are_escaped() {
        let m = Value::map([("v", Value::from("a\"b"))]);
        assert_eq!(
            render("<input th:value=\"${v}\">", &m).unwrap(),
            "<input value=\"a&quot;b\">"
        );
    }

    // --- structure ---------------------------------------------------------

    #[test]
    fn void_elements_have_no_closing_tag() {
        let m = Value::map([("src", Value::from("/a.png"))]);
        assert_eq!(
            render("<img th:src=\"${src}\">", &m).unwrap(),
            "<img src=\"/a.png\">"
        );
    }

    #[test]
    fn nested_elements_and_static_attrs_survive() {
        let m = Value::map([("name", Value::from("Alice"))]);
        let out = render(
            "<div class=\"u\"><span th:text=\"${name}\">n</span></div>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<div class=\"u\"><span>Alice</span></div>");
    }

    #[test]
    fn the_th_namespace_declaration_is_stripped() {
        let out = render(
            "<html xmlns:th=\"http://www.thymeleaf.org\"><body>hi</body></html>",
            &Value::Null,
        )
        .unwrap();
        assert_eq!(out, "<html><body>hi</body></html>");
    }

    // --- caching + a realistic page ---------------------------------------

    #[test]
    fn add_compiles_once_and_render_reuses_it() {
        let mut engine = Kernleaf::new();
        engine
            .add("page", "<h1 th:text=\"${name}\">n</h1>")
            .unwrap();
        assert!(engine.is_compiled("page"));
        let a = engine
            .render("page", &Value::map([("name", Value::from("A"))]))
            .unwrap();
        let b = engine
            .render("page", &Value::map([("name", Value::from("B"))]))
            .unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("<h1>A</h1>", "<h1>B</h1>"));
    }

    #[test]
    fn a_realistic_user_list_page() {
        let mut engine = Kernleaf::new();
        engine
            .add(
                "users",
                "<h1 th:text=\"${title}\">T</h1><ul>\
                 <li th:each=\"u : ${users}\" th:text=\"${u.name}\">name</li>\
                 </ul>",
            )
            .unwrap();
        let model = Value::map([
            ("title", Value::from("Team")),
            (
                "users",
                Value::seq([
                    Value::map([("name", Value::from("Alice"))]),
                    Value::map([("name", Value::from("Bob"))]),
                ]),
            ),
        ]);
        assert_eq!(
            engine.render("users", &model).unwrap(),
            "<h1>Team</h1><ul><li>Alice</li><li>Bob</li></ul>"
        );
    }

    #[test]
    fn rendering_an_unknown_template_errors() {
        assert!(Kernleaf::new().render("nope", &Value::Null).is_err());
    }

    // --- expressions through the attributes (slice B, end to end) ----------

    #[test]
    fn th_if_with_a_comparison() {
        let t = "<div th:if=\"${age} >= 18\">adult</div>";
        assert_eq!(
            render(t, &Value::map([("age", Value::from(20))])).unwrap(),
            "<div>adult</div>"
        );
        assert_eq!(
            render(t, &Value::map([("age", Value::from(15))])).unwrap(),
            ""
        );
    }

    #[test]
    fn th_text_with_a_ternary() {
        let t = "<span th:text=\"${admin} ? 'Admin' : 'User'\">role</span>";
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(true))])).unwrap(),
            "<span>Admin</span>"
        );
        assert_eq!(
            render(t, &Value::map([("admin", Value::from(false))])).unwrap(),
            "<span>User</span>"
        );
    }

    #[test]
    fn th_text_with_arithmetic_and_concatenation() {
        let m = Value::map([("n", Value::from(3))]);
        assert_eq!(
            render("<b th:text=\"${n} * 2\">x</b>", &m).unwrap(),
            "<b>6</b>"
        );
        assert_eq!(
            render("<b th:text=\"'#' + ${n}\">x</b>", &m).unwrap(),
            "<b>#3</b>"
        );
    }

    #[test]
    fn th_text_with_literal_substitution() {
        let m = Value::map([("name", Value::from("Alice")), ("n", Value::from(2))]);
        let out = render("<p th:text=\"|Hi ${name}, ${n} msgs|\">x</p>", &m).unwrap();
        assert_eq!(out, "<p>Hi Alice, 2 msgs</p>");
    }

    #[test]
    fn a_ternary_result_is_still_escaped() {
        // The XSS gate must hold through an expression, not just a bare variable.
        let m = Value::map([("evil", Value::from("<x>"))]);
        let out = render("<p th:text=\"true ? ${evil} : 'safe'\">x</p>", &m).unwrap();
        assert_eq!(out, "<p>&lt;x&gt;</p>");
    }

    #[test]
    fn th_each_still_works_with_the_expression_engine() {
        let m = Value::map([("xs", Value::seq([Value::from(1), Value::from(2)]))]);
        assert_eq!(
            render("<li th:each=\"x : ${xs}\" th:text=\"${x} + 10\">n</li>", &m).unwrap(),
            "<li>11</li><li>12</li>"
        );
    }

    // --- @{…} URLs and #{…} messages through the attributes (slice C) ------

    #[test]
    fn th_href_builds_a_url() {
        let m = Value::map([("id", Value::from(42))]);
        let out = render(
            "<a th:href=\"@{/users/{id}(id=${id}, ref='home')}\">go</a>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<a href=\"/users/42?ref=home\">go</a>");
    }

    #[test]
    fn th_text_resolves_a_message() {
        let mut engine = Kernleaf::new();
        engine.message("greeting", "Xin chào, {0}!");
        engine
            .add("t", "<h1 th:text=\"#{greeting(${name})}\">hi</h1>")
            .unwrap();
        let m = Value::map([("name", Value::from("Minh"))]);
        assert_eq!(engine.render("t", &m).unwrap(), "<h1>Xin chào, Minh!</h1>");
    }

    #[test]
    fn a_message_value_is_still_html_escaped_in_th_text() {
        // The message text is data too — escaping applies on top of resolution.
        let mut engine = Kernleaf::new();
        engine.message("raw", "<b>{0}</b>");
        engine.add("t", "<p th:text=\"#{raw('x')}\">y</p>").unwrap();
        assert_eq!(
            engine.render("t", &Value::Null).unwrap(),
            "<p>&lt;b&gt;x&lt;/b&gt;</p>"
        );
    }

    // --- th:inline / [[…]] context-aware escaping (slice E) ---------------

    #[test]
    fn inline_expression_is_html_escaped_by_default() {
        let m = Value::map([("x", Value::from("<b>"))]);
        assert_eq!(
            render("<p>Hi [[${x}]]!</p>", &m).unwrap(),
            "<p>Hi &lt;b&gt;!</p>"
        );
    }

    #[test]
    fn inline_raw_expression_is_not_escaped() {
        let m = Value::map([("x", Value::from("<b>ok</b>"))]);
        assert_eq!(render("<p>[(${x})]</p>", &m).unwrap(), "<p><b>ok</b></p>");
    }

    #[test]
    fn plain_text_stays_a_fast_text_node() {
        // No inline markers → Dom::Text, no per-render scan (the discipline).
        assert_eq!(
            render("<p>just plain text &amp; ok</p>", &Value::Null).unwrap(),
            "<p>just plain text &amp; ok</p>"
        );
    }

    #[test]
    fn javascript_inline_escapes_for_a_script_context() {
        // A value cannot break out of the JS string or close the <script>.
        let m = Value::map([("name", Value::from("a'<b>"))]);
        let out = render(
            "<script th:inline=\"javascript\">var n='[[${name}]]';</script>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<script>var n='a\\'\\u003Cb\\u003E';</script>");
    }

    #[test]
    fn css_inline_escapes_for_a_style_context() {
        let m = Value::map([("color", Value::from("red;}"))]);
        let out = render(
            "<style th:inline=\"css\">.x{color:[[${color}]]}</style>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<style>.x{color:red\\3B \\7D }</style>");
    }

    #[test]
    fn the_inline_mode_is_inherited_by_descendants() {
        let m = Value::map([("x", Value::from("<"))]);
        let out = render(
            "<div th:inline=\"javascript\"><span>[[${x}]]</span></div>",
            &m,
        )
        .unwrap();
        assert_eq!(out, "<div><span>\\u003C</span></div>");
    }

    // --- th:authorize + auto-CSRF (slice F) --------------------------------

    struct MockAuth {
        authed: bool,
        roles: &'static [&'static str],
    }
    impl kernway_core::security::Authorization for MockAuth {
        fn is_authenticated(&self) -> bool {
            self.authed
        }
        fn has_role(&self, role: &str) -> bool {
            self.roles.contains(&role)
        }
    }

    #[test]
    fn th_authorize_has_role() {
        let mut e = Kernleaf::new();
        e.add("t", "<div th:authorize=\"hasRole('ADMIN')\">panel</div>")
            .unwrap();
        let admin = MockAuth {
            authed: true,
            roles: &["ADMIN"],
        };
        let user = MockAuth {
            authed: true,
            roles: &["USER"],
        };
        assert_eq!(
            e.render_with("t", &Value::Null, &RenderContext::new().authorize(&admin))
                .unwrap(),
            "<div>panel</div>"
        );
        assert_eq!(
            e.render_with("t", &Value::Null, &RenderContext::new().authorize(&user))
                .unwrap(),
            ""
        );
    }

    #[test]
    fn th_authorize_is_fail_closed_without_a_context() {
        // A plain render() has no security context → anonymous → denied.
        let mut e = Kernleaf::new();
        e.add("t", "<div th:authorize=\"hasRole('ADMIN')\">secret</div>")
            .unwrap();
        assert_eq!(e.render("t", &Value::Null).unwrap(), "");
    }

    #[test]
    fn th_authorize_permit_deny_and_authenticated() {
        let mut e = Kernleaf::new();
        e.add("permit", "<p th:authorize=\"permitAll\">ok</p>")
            .unwrap();
        e.add("deny", "<p th:authorize=\"denyAll\">no</p>").unwrap();
        e.add("auth", "<p th:authorize=\"isAuthenticated()\">hi</p>")
            .unwrap();
        e.add("anyrole", "<p th:authorize=\"hasAnyRole('A','B')\">y</p>")
            .unwrap();

        let anon = MockAuth {
            authed: false,
            roles: &[],
        };
        let logged = MockAuth {
            authed: true,
            roles: &["B"],
        };
        let ctx_anon = RenderContext::new().authorize(&anon);
        let ctx_logged = RenderContext::new().authorize(&logged);

        assert_eq!(
            e.render_with("permit", &Value::Null, &ctx_anon).unwrap(),
            "<p>ok</p>"
        );
        assert_eq!(
            e.render_with("deny", &Value::Null, &ctx_logged).unwrap(),
            ""
        );
        assert_eq!(e.render_with("auth", &Value::Null, &ctx_anon).unwrap(), "");
        assert_eq!(
            e.render_with("auth", &Value::Null, &ctx_logged).unwrap(),
            "<p>hi</p>"
        );
        assert_eq!(
            e.render_with("anyrole", &Value::Null, &ctx_logged).unwrap(),
            "<p>y</p>"
        );
    }

    #[test]
    fn auto_csrf_injects_into_a_post_form() {
        let mut e = Kernleaf::new();
        e.add(
            "t",
            "<form method=\"post\" action=\"/save\"><button>Go</button></form>",
        )
        .unwrap();
        let out = e
            .render_with("t", &Value::Null, &RenderContext::new().csrf("tok123"))
            .unwrap();
        assert_eq!(
            out,
            "<form method=\"post\" action=\"/save\">\
             <input type=\"hidden\" name=\"_csrf\" value=\"tok123\">\
             <button>Go</button></form>"
        );
    }

    #[test]
    fn no_csrf_on_get_forms_or_without_a_token() {
        let mut e = Kernleaf::new();
        e.add("get", "<form method=\"get\"><button>Go</button></form>")
            .unwrap();
        e.add("post", "<form method=\"post\"><button>Go</button></form>")
            .unwrap();
        // A GET form, even with a token → no injection (nothing to protect).
        assert!(!e
            .render_with("get", &Value::Null, &RenderContext::new().csrf("t"))
            .unwrap()
            .contains("_csrf"));
        // A POST form with no token → no injection (nothing to inject).
        assert!(!e.render("post", &Value::Null).unwrap().contains("_csrf"));
    }

    // --- fragments: th:fragment / th:insert / th:replace (slice G) --------

    #[test]
    fn th_replace_swaps_the_host_for_the_fragment() {
        let mut e = Kernleaf::new();
        e.add("frags", "<div th:fragment=\"header\"><h1>Site</h1></div>")
            .unwrap();
        e.add(
            "page",
            "<body><div th:replace=\"frags :: header\">placeholder</div></body>",
        )
        .unwrap();
        assert_eq!(
            e.render("page", &Value::Null).unwrap(),
            "<body><div><h1>Site</h1></div></body>"
        );
    }

    #[test]
    fn th_insert_puts_the_fragment_inside_the_host() {
        let mut e = Kernleaf::new();
        e.add("frags", "<span th:fragment=\"label\">Hello</span>")
            .unwrap();
        e.add("page", "<div th:insert=\"frags :: label\">x</div>")
            .unwrap();
        assert_eq!(
            e.render("page", &Value::Null).unwrap(),
            "<div><span>Hello</span></div>"
        );
    }

    #[test]
    fn a_fragment_sees_the_model() {
        let mut e = Kernleaf::new();
        e.add(
            "frags",
            "<h1 th:fragment=\"title\" th:text=\"${name}\">x</h1>",
        )
        .unwrap();
        e.add("page", "<div th:replace=\"frags :: title\">x</div>")
            .unwrap();
        let m = Value::map([("name", Value::from("Home"))]);
        assert_eq!(e.render("page", &m).unwrap(), "<h1>Home</h1>");
    }

    #[test]
    fn a_same_template_reference_resolves() {
        // The fragment definition also renders in place — as Thymeleaf does when a
        // template both defines and uses a fragment.
        let mut e = Kernleaf::new();
        e.add(
            "t",
            "<i th:fragment=\"x\">hi</i><b th:replace=\":: x\">z</b>",
        )
        .unwrap();
        assert_eq!(e.render("t", &Value::Null).unwrap(), "<i>hi</i><i>hi</i>");
    }

    #[test]
    fn a_whole_template_reference() {
        let mut e = Kernleaf::new();
        e.add("part", "<p>partial</p>").unwrap();
        e.add("page", "<main th:insert=\"part\">x</main>").unwrap();
        assert_eq!(
            e.render("page", &Value::Null).unwrap(),
            "<main><p>partial</p></main>"
        );
    }

    #[test]
    fn the_tilde_brace_wrapper_is_accepted() {
        let mut e = Kernleaf::new();
        e.add("frags", "<p th:fragment=\"f\">ok</p>").unwrap();
        e.add("page", "<div th:replace=\"~{frags :: f}\">x</div>")
            .unwrap();
        assert_eq!(e.render("page", &Value::Null).unwrap(), "<p>ok</p>");
    }

    #[test]
    fn a_fragment_cycle_errors_instead_of_looping() {
        // Fragment "a" inserts itself; rendering hits the depth cap and errors
        // rather than looping forever.
        let mut e = Kernleaf::new();
        e.add(
            "t",
            "<div th:fragment=\"a\"><div th:insert=\":: a\">x</div></div>",
        )
        .unwrap();
        assert!(e.render("t", &Value::Null).is_err());
    }

    #[test]
    fn th_with_a_utility_object() {
        // slice D end to end: a utility call inside th:text, and one in th:if.
        let m = Value::map([
            ("name", Value::from("alice")),
            ("items", Value::seq([Value::from(1), Value::from(2)])),
        ]);
        assert_eq!(
            render(
                "<span th:text=\"#strings.capitalize(${name})\">x</span>",
                &m
            )
            .unwrap(),
            "<span>Alice</span>"
        );
        assert_eq!(
            render("<p th:if=\"#lists.size(${items}) > 1\">many</p>", &m).unwrap(),
            "<p>many</p>"
        );
    }
}
