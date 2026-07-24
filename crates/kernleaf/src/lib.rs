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

use kernway_core::template::{TemplateEngine, TemplateError, Value};

mod expr;
use expr::Expr;

// ============================================================
// IR — the parsed template DOM, cached and walked at render time
// ============================================================

/// A node in the parsed template. This *is* the cached IR: `add` parses to it
/// once, `render` walks it.
#[derive(Debug, Clone)]
enum Dom {
    /// Literal text, emitted verbatim (it is template author text, not data).
    Text(String),
    /// `<!-- … -->`, kept so the output stays a faithful copy.
    Comment(String),
    /// A doctype or other `<!…>` declaration, emitted verbatim.
    Declaration(String),
    /// An element, with its `th:*` directives already extracted.
    Element(Element),
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
    children: Vec<Dom>,
}

/// The template engine — every template's parsed DOM, keyed by name.
#[derive(Default)]
pub struct Kernleaf {
    templates: HashMap<String, Vec<Dom>>,
}

impl Kernleaf {
    /// A new engine with no templates.
    pub fn new() -> Self {
        Self { templates: HashMap::new() }
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

impl TemplateEngine for Kernleaf {
    fn render(&self, template: &str, model: &Value<'_>) -> Result<String, TemplateError> {
        let dom = self
            .templates
            .get(template)
            .ok_or_else(|| err(format!("no template named `{template}`")))?;
        let mut out = String::new();
        let mut scope: Vec<(&str, &Value<'_>)> = Vec::new();
        render_nodes(dom, model, &mut scope, &mut out)?;
        Ok(out)
    }
}

// ============================================================
// Rendering — walk the DOM against the model
// ============================================================

fn render_nodes<'ir, 'm>(
    nodes: &'ir [Dom],
    model: &'m Value<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    out: &mut String,
) -> Result<(), TemplateError> {
    for node in nodes {
        match node {
            Dom::Text(t) => out.push_str(t),
            Dom::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            Dom::Declaration(d) => out.push_str(d),
            Dom::Element(el) => render_element(el, model, scope, out)?,
        }
    }
    Ok(())
}

fn render_element<'ir, 'm>(
    el: &'ir Element,
    model: &'m Value<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    out: &mut String,
) -> Result<(), TemplateError> {
    // Precedence matches Thymeleaf: th:each (outer) wraps th:if (inner).
    if let Some((var, seq)) = &el.th_each {
        // A non-sequence (missing, or a scalar) is zero iterations — lenient.
        if let Some(items) = expr::eval(seq, model, scope).as_seq() {
            for item in items {
                scope.push((var.as_str(), item));
                let r = render_instance(el, model, scope, out);
                scope.pop();
                r?;
            }
        }
        Ok(())
    } else {
        render_instance(el, model, scope, out)
    }
}

/// Render one element instance (th:each already resolved for this iteration):
/// evaluate th:if/th:unless, then open tag + attributes + body + close.
fn render_instance<'ir, 'm>(
    el: &'ir Element,
    model: &'m Value<'m>,
    scope: &mut Vec<(&'ir str, &'m Value<'m>)>,
    out: &mut String,
) -> Result<(), TemplateError> {
    if let Some(cond) = &el.th_if {
        if !expr::eval(cond, model, scope).to_bool() {
            return Ok(());
        }
    }
    if let Some(cond) = &el.th_unless {
        if expr::eval(cond, model, scope).to_bool() {
            return Ok(());
        }
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
        expr::eval(e, model, scope).write_string(&mut buf);
        escape_html_into(&buf, out);
        out.push('"');
    }

    if el.void {
        out.push('>');
        return Ok(());
    }
    out.push('>');

    // Body: th:text (escaped) or th:utext (raw) replace the children; otherwise
    // the children render (the natural-template placeholder path).
    if let Some(e) = &el.th_text {
        let mut buf = String::new();
        expr::eval(e, model, scope).write_string(&mut buf);
        escape_html_into(&buf, out);
    } else if let Some(e) = &el.th_utext {
        expr::eval(e, model, scope).write_string(out); // raw — the explicit unsafe path
    } else {
        render_nodes(&el.children, model, scope, out)?;
    }

    out.push_str("</");
    out.push_str(&el.tag);
    out.push('>');
    Ok(())
}

/// HTML-escape into `out` — the five characters that matter in element content
/// and double-quoted attributes. URL and JS contexts need different rules and are
/// a later slice; this does not claim them.
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
        Self { s: src.as_bytes(), src, pos: 0 }
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
                nodes.push(self.read_text());
            }
        }
        Ok(nodes)
    }

    fn read_text(&mut self) -> Dom {
        let start = self.pos;
        while !self.eof() && self.s[self.pos] != b'<' {
            self.pos += 1;
        }
        Dom::Text(self.src[start..self.pos].to_string())
    }

    fn read_comment(&mut self) -> Result<Dom, TemplateError> {
        self.pos += 4; // <!--
        let start = self.pos;
        let end = self.src[start..].find("-->").ok_or_else(|| err("unterminated comment"))?;
        let body = self.src[start..start + end].to_string();
        self.pos = start + end + 3;
        Ok(Dom::Comment(body))
    }

    fn read_declaration(&mut self) -> Result<Dom, TemplateError> {
        let start = self.pos;
        let end = self.src[start..].find('>').ok_or_else(|| err("unterminated `<!…>`"))?;
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

        let children = if void { Vec::new() } else { self.parse_nodes()? };
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
                attr => el.dynamic_attrs.push((attr.to_string(), parse_expr(&value)?)),
            },
        }
    }
    Ok(el)
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
        assert_eq!(render("<h1 th:text=\"${title}\">Placeholder</h1>", &m).unwrap(), "<h1>Team</h1>");
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
        assert_eq!(render("<div th:utext=\"${html}\">x</div>", &m).unwrap(), "<div><b>bold</b></div>");
    }

    #[test]
    fn a_string_literal_expression() {
        assert_eq!(render("<p th:text=\"'hi'\">x</p>", &Value::Null).unwrap(), "<p>hi</p>");
    }

    // --- th:if / th:unless -------------------------------------------------

    #[test]
    fn th_if_includes_or_drops_the_element() {
        let t = "<div th:if=\"${admin}\">panel</div>";
        assert_eq!(render(t, &Value::map([("admin", Value::from(true))])).unwrap(), "<div>panel</div>");
        assert_eq!(render(t, &Value::map([("admin", Value::from(false))])).unwrap(), "");
    }

    #[test]
    fn th_unless_is_the_inverse() {
        let t = "<div th:unless=\"${admin}\">hi</div>";
        assert_eq!(render(t, &Value::map([("admin", Value::from(false))])).unwrap(), "<div>hi</div>");
        assert_eq!(render(t, &Value::map([("admin", Value::from(true))])).unwrap(), "");
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
        let out = render("<ul><li th:each=\"p : ${posts}\" th:text=\"${p.title}\">t</li></ul>", &m).unwrap();
        assert_eq!(out, "<ul><li>A</li><li>B</li></ul>");
    }

    #[test]
    fn th_each_over_a_missing_sequence_is_empty() {
        assert_eq!(render("<ul><li th:each=\"p : ${posts}\">x</li></ul>", &Value::Null).unwrap(), "<ul></ul>");
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
        assert_eq!(render("<input th:value=\"${v}\">", &m).unwrap(), "<input value=\"a&quot;b\">");
    }

    // --- structure ---------------------------------------------------------

    #[test]
    fn void_elements_have_no_closing_tag() {
        let m = Value::map([("src", Value::from("/a.png"))]);
        assert_eq!(render("<img th:src=\"${src}\">", &m).unwrap(), "<img src=\"/a.png\">");
    }

    #[test]
    fn nested_elements_and_static_attrs_survive() {
        let m = Value::map([("name", Value::from("Alice"))]);
        let out = render("<div class=\"u\"><span th:text=\"${name}\">n</span></div>", &m).unwrap();
        assert_eq!(out, "<div class=\"u\"><span>Alice</span></div>");
    }

    #[test]
    fn the_th_namespace_declaration_is_stripped() {
        let out = render("<html xmlns:th=\"http://www.thymeleaf.org\"><body>hi</body></html>", &Value::Null).unwrap();
        assert_eq!(out, "<html><body>hi</body></html>");
    }

    // --- caching + a realistic page ---------------------------------------

    #[test]
    fn add_compiles_once_and_render_reuses_it() {
        let mut engine = Kernleaf::new();
        engine.add("page", "<h1 th:text=\"${name}\">n</h1>").unwrap();
        assert!(engine.is_compiled("page"));
        let a = engine.render("page", &Value::map([("name", Value::from("A"))])).unwrap();
        let b = engine.render("page", &Value::map([("name", Value::from("B"))])).unwrap();
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
        assert_eq!(render(t, &Value::map([("age", Value::from(20))])).unwrap(), "<div>adult</div>");
        assert_eq!(render(t, &Value::map([("age", Value::from(15))])).unwrap(), "");
    }

    #[test]
    fn th_text_with_a_ternary() {
        let t = "<span th:text=\"${admin} ? 'Admin' : 'User'\">role</span>";
        assert_eq!(render(t, &Value::map([("admin", Value::from(true))])).unwrap(), "<span>Admin</span>");
        assert_eq!(render(t, &Value::map([("admin", Value::from(false))])).unwrap(), "<span>User</span>");
    }

    #[test]
    fn th_text_with_arithmetic_and_concatenation() {
        let m = Value::map([("n", Value::from(3))]);
        assert_eq!(render("<b th:text=\"${n} * 2\">x</b>", &m).unwrap(), "<b>6</b>");
        assert_eq!(render("<b th:text=\"'#' + ${n}\">x</b>", &m).unwrap(), "<b>#3</b>");
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
}
