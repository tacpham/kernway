//! The Standard Expression language (KEP-0003 slice B).
//!
//! An attribute value like `${user.age} >= 18 ? 'adult' : 'minor'` is parsed once
//! into an [`Expr`] AST and evaluated against the [`Value`] model. This is the
//! Thymeleaf Standard Expression core: variable paths, literals, arithmetic,
//! comparison, boolean logic, the ternary and elvis operators, and `|literal
//! substitution ${with} embeds|`.
//!
//! `${...}` and `*{...}` are transparent wrappers around a sub-expression, and a
//! bare identifier is a variable path too — so `${x} > 5`, `${x > 5}`, and
//! `x > 5` all mean the same thing. That is a small superset of Thymeleaf (which
//! wants the `${}`), chosen because it is simpler and accepts every valid Standard
//! Expression.
//!
//! Not here (later slices): `@{...}` URLs, `#{...}` messages, `#utility` objects,
//! method calls. See the kernleaf charter.

use std::collections::HashMap;
use std::fmt::Write as _;

use kernway_core::security::Authorization;
use kernway_core::template::Value;

/// The immutable render environment: the model plus the resources a template may
/// reach — the message bundle (`#{...}`), the context path for `@{...}` URLs, the
/// authorization facts for `th:authorize`, and the CSRF token to inject into
/// forms. The loop scope is passed separately because it changes as `th:each`
/// pushes and pops bindings.
pub struct Env<'m> {
    pub model: &'m Value<'m>,
    pub messages: &'m HashMap<String, String>,
    pub context_path: &'m str,
    pub authz: &'m dyn Authorization,
    pub csrf: Option<&'m str>,
}

/// A parsed expression — the AST cached in the template DOM.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `${a.b}` / bare `a.b` — a dotted lookup against loop scope then the model.
    Var(Vec<String>),
    /// `'text'`.
    Str(String),
    /// `42`, `3.14`.
    Num(f64),
    /// `true` / `false`.
    Bool(bool),
    /// `null`.
    Null,
    /// `|text ${x} more|` — literal substitution, evaluated as concatenation.
    Template(Vec<Expr>),
    /// `@{/path/{v}(v=${x}, q=${y})}` — a URL. Path variables `{v}` are
    /// substituted (URL-encoded) into `base`; the rest become query parameters.
    Url { base: String, params: Vec<(String, Expr)> },
    /// `#{key(arg0, arg1)}` — an i18n message, with `{0}`/`{1}` placeholders
    /// filled from the arguments.
    Msg { key: String, args: Vec<Expr> },
    /// `#strings.toUpperCase(${x})` — a utility-object method call. A *separate*
    /// branch from `Var`, so a plain `${var}` never pays for method dispatch.
    Util { object: String, method: String, args: Vec<Expr> },
    /// `-x`, `not x`, `!x`.
    Unary(UnOp, Box<Expr>),
    /// A binary operator.
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `cond ? then : els`.
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `a ?: b` — `a` if truthy, else `b`.
    Elvis(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// The result of evaluating an expression. A plain variable stays a borrow into
/// the model (`Ref`); computed values are owned.
pub enum EvalVal<'m> {
    Ref(&'m Value<'m>),
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

impl<'m> EvalVal<'m> {
    /// Truthiness — the rule for `th:if`, `and`/`or`, and elvis.
    pub fn to_bool(&self) -> bool {
        match self {
            EvalVal::Ref(v) => v.is_truthy(),
            EvalVal::Str(s) => !s.is_empty(),
            EvalVal::Num(n) => *n != 0.0,
            EvalVal::Bool(b) => *b,
            EvalVal::Null => false,
        }
    }

    /// The sequence a `Ref` points at, for `th:each`; `None` for anything else.
    pub fn as_seq(&self) -> Option<&'m [Value<'m>]> {
        match self {
            EvalVal::Ref(Value::Seq(xs)) => Some(xs),
            _ => None,
        }
    }

    /// Append the text form to `out` (unescaped — the caller escapes).
    pub fn write_string(&self, out: &mut String) {
        match self {
            EvalVal::Ref(v) => value_to_string(v, out),
            EvalVal::Str(s) => out.push_str(s),
            EvalVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            EvalVal::Num(n) => write_num(*n, out),
            EvalVal::Null => {}
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            EvalVal::Num(n) => Some(*n),
            EvalVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            EvalVal::Ref(Value::Int(n)) => Some(*n as f64),
            EvalVal::Ref(Value::Float(f)) => Some(*f),
            EvalVal::Ref(Value::Str(s)) => s.trim().parse().ok(),
            EvalVal::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn to_text(&self) -> String {
        let mut s = String::new();
        self.write_string(&mut s);
        s
    }

    fn is_null(&self) -> bool {
        matches!(self, EvalVal::Null | EvalVal::Ref(Value::Null))
    }
}

fn write_num(n: f64, out: &mut String) {
    // Render a whole number without a trailing `.0`, matching Thymeleaf: `1 + 1`
    // is "2", not "2.0".
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

fn value_to_string(value: &Value<'_>, out: &mut String) {
    match value {
        Value::Null | Value::Seq(_) | Value::Map(_) => {}
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Float(f) => write_num(*f, out),
        Value::Str(s) => out.push_str(s),
    }
}

// ============================================================
// Evaluation
// ============================================================

/// Evaluate `e` against the environment and the current loop scope.
pub fn eval<'m>(e: &Expr, env: &Env<'m>, scope: &[(&str, &'m Value<'m>)]) -> EvalVal<'m> {
    match e {
        Expr::Var(p) => resolve(p, env.model, scope).map(EvalVal::Ref).unwrap_or(EvalVal::Null),
        Expr::Str(s) => EvalVal::Str(s.clone()),
        Expr::Num(n) => EvalVal::Num(*n),
        Expr::Bool(b) => EvalVal::Bool(*b),
        Expr::Null => EvalVal::Null,
        Expr::Template(parts) => {
            let mut s = String::new();
            for part in parts {
                eval(part, env, scope).write_string(&mut s);
            }
            EvalVal::Str(s)
        }
        Expr::Url { base, params } => EvalVal::Str(eval_url(base, params, env, scope)),
        Expr::Msg { key, args } => EvalVal::Str(eval_msg(key, args, env, scope)),
        Expr::Util { object, method, args } => {
            let vals: Vec<EvalVal> = args.iter().map(|a| eval(a, env, scope)).collect();
            eval_util(object, method, &vals)
        }
        Expr::Unary(op, x) => {
            let v = eval(x, env, scope);
            match op {
                UnOp::Not => EvalVal::Bool(!v.to_bool()),
                UnOp::Neg => EvalVal::Num(-v.as_num().unwrap_or(0.0)),
            }
        }
        Expr::Ternary(c, a, b) => {
            if eval(c, env, scope).to_bool() {
                eval(a, env, scope)
            } else {
                eval(b, env, scope)
            }
        }
        Expr::Elvis(a, b) => {
            let av = eval(a, env, scope);
            if av.to_bool() {
                av
            } else {
                eval(b, env, scope)
            }
        }
        Expr::Binary(op, l, r) => {
            let lv = eval(l, env, scope);
            let rv = eval(r, env, scope);
            eval_binary(*op, lv, rv)
        }
    }
}

/// Build a URL: substitute `{name}` path variables (URL-encoded), append the rest
/// as a query string, and prepend the context path. Encoding a value is the URL
/// escape context — distinct from HTML escaping.
fn eval_url<'m>(
    base: &str,
    params: &[(String, Expr)],
    env: &Env<'m>,
    scope: &[(&str, &'m Value<'m>)],
) -> String {
    let mut url = base.to_string();
    let mut query: Vec<(String, String)> = Vec::new();
    for (name, vexpr) in params {
        let mut val = String::new();
        eval(vexpr, env, scope).write_string(&mut val);
        let placeholder = format!("{{{name}}}");
        if url.contains(&placeholder) {
            url = url.replace(&placeholder, &url_encode(&val));
        } else {
            query.push((name.clone(), val));
        }
    }
    if !query.is_empty() {
        url.push('?');
        for (i, (k, v)) in query.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str(&url_encode(k));
            url.push('=');
            url.push_str(&url_encode(v));
        }
    }
    if env.context_path.is_empty() {
        url
    } else {
        format!("{}{}", env.context_path, url)
    }
}

/// Resolve a message: look up the key, fill `{0}`/`{1}` from the arguments. A
/// missing key renders `??key??`, as Thymeleaf does, so it is visible in the page.
fn eval_msg<'m>(
    key: &str,
    args: &[Expr],
    env: &Env<'m>,
    scope: &[(&str, &'m Value<'m>)],
) -> String {
    let mut text = match env.messages.get(key) {
        Some(t) => t.clone(),
        None => return format!("??{key}??"),
    };
    for (i, arg) in args.iter().enumerate() {
        let mut a = String::new();
        eval(arg, env, scope).write_string(&mut a);
        text = text.replace(&format!("{{{i}}}"), &a);
    }
    text
}

/// Dispatch a utility-object method. A curated, extensible subset of Thymeleaf's
/// `#strings`/`#numbers`/`#lists`/`#bools`; an unknown call is `Null` (visible as
/// empty) rather than a hard error, so a template is not broken by a typo in a
/// rarely-used helper. Reached only for a `#util` node — never for a plain `${x}`.
fn eval_util<'m>(object: &str, method: &str, args: &[EvalVal<'m>]) -> EvalVal<'m> {
    let text = |i: usize| args.get(i).map(EvalVal::to_text).unwrap_or_default();
    let num = |i: usize| args.get(i).and_then(EvalVal::as_num).unwrap_or(0.0);
    match (object, method) {
        // #strings
        ("strings", "toUpperCase") => EvalVal::Str(text(0).to_uppercase()),
        ("strings", "toLowerCase") => EvalVal::Str(text(0).to_lowercase()),
        ("strings", "trim") => EvalVal::Str(text(0).trim().to_string()),
        ("strings", "length") => EvalVal::Num(text(0).chars().count() as f64),
        ("strings", "isEmpty") => EvalVal::Bool(text(0).is_empty()),
        ("strings", "contains") => EvalVal::Bool(text(0).contains(&text(1))),
        ("strings", "capitalize") => EvalVal::Str(capitalize(&text(0))),
        ("strings", "defaultString") => {
            let s = text(0);
            EvalVal::Str(if s.is_empty() { text(1) } else { s })
        }
        // #numbers
        ("numbers", "abs") => EvalVal::Num(num(0).abs()),
        ("numbers", "formatInteger") => EvalVal::Str(format_decimal(num(0), args.get(1).and_then(EvalVal::as_num).unwrap_or(1.0) as usize, 0)),
        ("numbers", "formatDecimal") => EvalVal::Str(format_decimal(
            num(0),
            args.get(1).and_then(EvalVal::as_num).unwrap_or(1.0) as usize,
            num(2) as usize,
        )),
        // #lists
        ("lists", "size") => EvalVal::Num(args.first().and_then(EvalVal::as_seq).map_or(0, <[_]>::len) as f64),
        ("lists", "isEmpty") => {
            EvalVal::Bool(args.first().and_then(EvalVal::as_seq).is_none_or(<[_]>::is_empty))
        }
        ("lists", "contains") => {
            let found = args
                .first()
                .and_then(EvalVal::as_seq)
                .map(|xs| {
                    let needle = text(1);
                    xs.iter().any(|v| {
                        let mut s = String::new();
                        value_to_string(v, &mut s);
                        s == needle
                    })
                })
                .unwrap_or(false);
            EvalVal::Bool(found)
        }
        // #bools
        ("bools", "isTrue") => EvalVal::Bool(args.first().map(EvalVal::to_bool).unwrap_or(false)),
        _ => EvalVal::Null,
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Format `n` with `decimals` fractional digits and at least `min_int` integer
/// digits (zero-padded). `#numbers.formatDecimal(3.14159, 1, 2)` → "3.14".
fn format_decimal(n: f64, min_int: usize, decimals: usize) -> String {
    let sign = if n < 0.0 { "-" } else { "" };
    let s = format!("{:.*}", decimals, n.abs());
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s, None),
    };
    let padded = if int_part.len() < min_int {
        format!("{}{}", "0".repeat(min_int - int_part.len()), int_part)
    } else {
        int_part
    };
    match frac_part {
        Some(f) => format!("{sign}{padded}.{f}"),
        None => format!("{sign}{padded}"),
    }
}

/// Percent-encode a string as a URL component (RFC 3986 unreserved kept).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn eval_binary<'m>(op: BinOp, l: EvalVal<'m>, r: EvalVal<'m>) -> EvalVal<'m> {
    match op {
        BinOp::Or => EvalVal::Bool(l.to_bool() || r.to_bool()),
        BinOp::And => EvalVal::Bool(l.to_bool() && r.to_bool()),
        BinOp::Eq => EvalVal::Bool(values_equal(&l, &r)),
        BinOp::Ne => EvalVal::Bool(!values_equal(&l, &r)),
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => {
            let ord = match (l.as_num(), r.as_num()) {
                (Some(a), Some(b)) => a.partial_cmp(&b),
                // Fall back to lexicographic string comparison.
                _ => Some(l.to_text().cmp(&r.to_text())),
            };
            let res = match (op, ord) {
                (_, None) => false,
                (BinOp::Gt, Some(o)) => o == std::cmp::Ordering::Greater,
                (BinOp::Lt, Some(o)) => o == std::cmp::Ordering::Less,
                (BinOp::Ge, Some(o)) => o != std::cmp::Ordering::Less,
                (BinOp::Le, Some(o)) => o != std::cmp::Ordering::Greater,
                _ => unreachable!(),
            };
            EvalVal::Bool(res)
        }
        BinOp::Add => match (l.as_num(), r.as_num()) {
            (Some(a), Some(b)) => EvalVal::Num(a + b),
            // `+` on non-numbers is string concatenation, as in Thymeleaf.
            _ => {
                let mut s = l.to_text();
                r.write_string(&mut s);
                EvalVal::Str(s)
            }
        },
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            let a = l.as_num().unwrap_or(0.0);
            let b = r.as_num().unwrap_or(0.0);
            EvalVal::Num(match op {
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => a % b,
                _ => unreachable!(),
            })
        }
    }
}

fn values_equal(l: &EvalVal<'_>, r: &EvalVal<'_>) -> bool {
    if l.is_null() || r.is_null() {
        return l.is_null() && r.is_null();
    }
    match (l.as_num(), r.as_num()) {
        (Some(a), Some(b)) => a == b,
        _ => l.to_text() == r.to_text(),
    }
}

/// Resolve a dotted path against the loop scope (which shadows) then the model.
fn resolve<'m>(
    path: &[String],
    model: &'m Value<'m>,
    scope: &[(&str, &'m Value<'m>)],
) -> Option<&'m Value<'m>> {
    let (head, tail) = path.split_first()?;
    let base = scope
        .iter()
        .rev()
        .find(|(name, _)| *name == head.as_str())
        .map(|(_, v)| *v)
        .or_else(|| model.get(head))?;
    let mut cur = base;
    for seg in tail {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

// ============================================================
// Tokenizing
// ============================================================

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String), // a dotted path
    TemplateRaw(String),
    UrlRaw(String), // the inside of @{ … }
    MsgRaw(String), // the inside of #{ … }
    True,
    False,
    Null,
    DollarOpen, // ${
    StarOpen,   // *{
    RBrace,     // }
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Gt,
    Lt,
    Ge,
    Le,
    EqEq,
    Ne,
    And,
    Or,
    Not,
    Question,
    Colon,
    Elvis, // ?:
    Comma,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut toks = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'\'' {
                    j += 1;
                }
                if j >= b.len() {
                    return Err("unterminated string literal".into());
                }
                toks.push(Tok::Str(s[start..j].to_string()));
                i = j + 1;
            }
            b'|' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'|' {
                    j += 1;
                }
                if j >= b.len() {
                    return Err("unterminated `|…|` literal substitution".into());
                }
                toks.push(Tok::TemplateRaw(s[start..j].to_string()));
                i = j + 1;
            }
            b'@' if i + 1 < b.len() && b[i + 1] == b'{' => {
                let (inner, next) = scan_braced(s, b, i + 1).ok_or("unclosed `@{`")?;
                toks.push(Tok::UrlRaw(inner));
                i = next;
            }
            b'#' if i + 1 < b.len() && b[i + 1] == b'{' => {
                let (inner, next) = scan_braced(s, b, i + 1).ok_or("unclosed `#{`")?;
                toks.push(Tok::MsgRaw(inner));
                i = next;
            }
            b'#' => {
                // A utility object like `#strings.toUpperCase` — `#` plus an
                // identifier path. (The `#{` message form was handled above.)
                let start = i;
                i += 1;
                while i < b.len() && is_ident_part(b[i]) {
                    i += 1;
                }
                toks.push(Tok::Ident(s[start..i].to_string()));
            }
            b',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            b'$' if i + 1 < b.len() && b[i + 1] == b'{' => {
                toks.push(Tok::DollarOpen);
                i += 2;
            }
            b'*' if i + 1 < b.len() && b[i + 1] == b'{' => {
                toks.push(Tok::StarOpen);
                i += 2;
            }
            b'}' => {
                toks.push(Tok::RBrace);
                i += 1;
            }
            b'(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            b'+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            b'>' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    toks.push(Tok::Le);
                    i += 2;
                } else {
                    toks.push(Tok::Lt);
                    i += 1;
                }
            }
            b'=' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    toks.push(Tok::EqEq);
                    i += 2;
                } else {
                    return Err("`=` must be `==`".into());
                }
            }
            b'!' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    toks.push(Tok::Not);
                    i += 1;
                }
            }
            b'?' => {
                if i + 1 < b.len() && b[i + 1] == b':' {
                    toks.push(Tok::Elvis);
                    i += 2;
                } else {
                    toks.push(Tok::Question);
                    i += 1;
                }
            }
            b':' => {
                toks.push(Tok::Colon);
                i += 1;
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                let mut seen_dot = false;
                while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b'.' && !seen_dot)) {
                    if b[i] == b'.' {
                        // A dot followed by a non-digit ends the number.
                        if i + 1 >= b.len() || !b[i + 1].is_ascii_digit() {
                            break;
                        }
                        seen_dot = true;
                    }
                    i += 1;
                }
                let n: f64 = s[start..i].parse().map_err(|_| format!("bad number `{}`", &s[start..i]))?;
                toks.push(Tok::Num(n));
            }
            _ if is_ident_start(c) => {
                let start = i;
                while i < b.len() && is_ident_part(b[i]) {
                    i += 1;
                }
                let word = &s[start..i];
                toks.push(match word {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "gt" => Tok::Gt,
                    "lt" => Tok::Lt,
                    "ge" => Tok::Ge,
                    "le" => Tok::Le,
                    "eq" => Tok::EqEq,
                    "ne" => Tok::Ne,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "null" => Tok::Null,
                    _ => Tok::Ident(word.to_string()),
                });
            }
            _ => return Err(format!("unexpected character `{}`", c as char)),
        }
    }
    Ok(toks)
}

/// Given `b[open] == '{'`, return the content up to the matching `}` (counting
/// nested braces, so a `${…}` inside does not close early) and the index after it.
fn scan_braced(s: &str, b: &[u8], open: usize) -> Option<(String, usize)> {
    let start = open + 1;
    let mut depth = 1;
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((s[start..j].to_string(), j + 1));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

// ============================================================
// Parsing — precedence-climbing recursive descent
// ============================================================

/// Parse an attribute value into an [`Expr`].
pub fn parse(s: &str) -> Result<Expr, String> {
    let toks = tokenize(s)?;
    if toks.is_empty() {
        return Err("empty expression".into());
    }
    let mut p = P { toks: &toks, pos: 0 };
    let e = p.ternary()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing tokens in `{s}`"));
    }
    Ok(e)
}

struct P<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Parse `(a, b, c)` argument lists — the `(` is already consumed.
    fn arg_list(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.ternary()?);
            if self.eat(&Tok::RParen) {
                break;
            }
            if !self.eat(&Tok::Comma) {
                return Err("expected `,` or `)` in argument list".into());
            }
        }
        Ok(args)
    }

    fn ternary(&mut self) -> Result<Expr, String> {
        let cond = self.or()?;
        if self.eat(&Tok::Elvis) {
            let alt = self.ternary()?;
            return Ok(Expr::Elvis(Box::new(cond), Box::new(alt)));
        }
        if self.eat(&Tok::Question) {
            let then = self.ternary()?;
            if !self.eat(&Tok::Colon) {
                return Err("ternary `?` without `:`".into());
            }
            let els = self.ternary()?;
            return Ok(Expr::Ternary(Box::new(cond), Box::new(then), Box::new(els)));
        }
        Ok(cond)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut e = self.and()?;
        while self.eat(&Tok::Or) {
            e = Expr::Binary(BinOp::Or, Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.equality()?;
        while self.eat(&Tok::And) {
            e = Expr::Binary(BinOp::And, Box::new(e), Box::new(self.equality()?));
        }
        Ok(e)
    }

    fn equality(&mut self) -> Result<Expr, String> {
        let mut e = self.comparison()?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.comparison()?));
        }
        Ok(e)
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut e = self.additive()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Ge) => BinOp::Ge,
                Some(Tok::Le) => BinOp::Le,
                _ => break,
            };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.additive()?));
        }
        Ok(e)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut e = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.multiplicative()?));
        }
        Ok(e)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Rem,
                _ => break,
            };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.unary()?));
        }
        Ok(e)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary(UnOp::Neg, Box::new(self.unary()?)));
        }
        if self.eat(&Tok::Not) {
            return Ok(Expr::Unary(UnOp::Not, Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let tok = self.peek().ok_or("unexpected end of expression")?.clone();
        match tok {
            Tok::Num(n) => {
                self.pos += 1;
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.pos += 1;
                Ok(Expr::Str(s))
            }
            Tok::True => {
                self.pos += 1;
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.pos += 1;
                Ok(Expr::Bool(false))
            }
            Tok::Null => {
                self.pos += 1;
                Ok(Expr::Null)
            }
            Tok::Ident(path) => {
                self.pos += 1;
                if let Some(util) = path.strip_prefix('#') {
                    // `#object.method(args)` — a utility call.
                    let (object, method) = util
                        .rsplit_once('.')
                        .ok_or_else(|| format!("utility `#{util}` needs a `.method`"))?;
                    if !self.eat(&Tok::LParen) {
                        return Err(format!("utility `#{util}` needs `(`"));
                    }
                    let args = self.arg_list()?;
                    Ok(Expr::Util { object: object.to_string(), method: method.to_string(), args })
                } else {
                    Ok(Expr::Var(split_path(&path)?))
                }
            }
            Tok::TemplateRaw(raw) => {
                self.pos += 1;
                Ok(parse_template(&raw)?)
            }
            Tok::UrlRaw(raw) => {
                self.pos += 1;
                Ok(parse_url(&raw)?)
            }
            Tok::MsgRaw(raw) => {
                self.pos += 1;
                Ok(parse_msg(&raw)?)
            }
            Tok::DollarOpen | Tok::StarOpen => {
                // `${ … }` / `*{ … }` — a transparent wrapper around an expression.
                self.pos += 1;
                let inner = self.ternary()?;
                if !self.eat(&Tok::RBrace) {
                    return Err("`${` without closing `}`".into());
                }
                Ok(inner)
            }
            Tok::LParen => {
                self.pos += 1;
                let inner = self.ternary()?;
                if !self.eat(&Tok::RParen) {
                    return Err("`(` without closing `)`".into());
                }
                Ok(inner)
            }
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

fn split_path(s: &str) -> Result<Vec<String>, String> {
    let path: Vec<String> = s.split('.').map(|seg| seg.trim().to_string()).collect();
    if path.iter().any(String::is_empty) {
        return Err(format!("malformed path `{s}`"));
    }
    Ok(path)
}

/// Parse the inside of a `@{…}`: `base` optionally followed by `(name=expr, …)`.
/// A `name` that appears as `{name}` in the base is a path variable; the rest are
/// query parameters.
fn parse_url(raw: &str) -> Result<Expr, String> {
    let raw = raw.trim();
    let (base, params_src) = match raw.find('(') {
        Some(open) if raw.ends_with(')') => (&raw[..open], &raw[open + 1..raw.len() - 1]),
        _ => (raw, ""),
    };
    let mut params = Vec::new();
    for part in split_top_commas(params_src) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part
            .split_once('=')
            .ok_or_else(|| format!("URL parameter needs `name=value`: `{part}`"))?;
        params.push((name.trim().to_string(), parse(value.trim())?));
    }
    Ok(Expr::Url { base: base.trim().to_string(), params })
}

/// Parse the inside of a `#{…}`: `key` optionally followed by `(arg, …)`.
fn parse_msg(raw: &str) -> Result<Expr, String> {
    let raw = raw.trim();
    let (key, args_src) = match raw.find('(') {
        Some(open) if raw.ends_with(')') => (&raw[..open], &raw[open + 1..raw.len() - 1]),
        _ => (raw, ""),
    };
    let key = key.trim();
    if key.is_empty() {
        return Err("empty message key `#{}`".into());
    }
    let mut args = Vec::new();
    for part in split_top_commas(args_src) {
        let part = part.trim();
        if !part.is_empty() {
            args.push(parse(part)?);
        }
    }
    Ok(Expr::Msg { key: key.to_string(), args })
}

/// Split on commas that are at the top level — not inside `${…}`, `(…)`, or a
/// `'…'` string. Used for `@{}`/`#{}` parameter lists.
fn split_top_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'\'' => in_str = !in_str,
            b'{' | b'(' if !in_str => depth += 1,
            b'}' | b')' if !in_str => depth -= 1,
            b',' if !in_str && depth == 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() || !s.is_empty() {
        parts.push(s[start..].to_string());
    }
    parts
}

/// Parse the inside of a `|…|`: literal text with `${…}` embeds → a `Template`.
fn parse_template(raw: &str) -> Result<Expr, String> {
    let mut parts = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find("${") {
        if open > 0 {
            parts.push(Expr::Str(rest[..open].to_string()));
        }
        let after = &rest[open + 2..];
        let close = after.find('}').ok_or("`${` without `}` in `|…|`")?;
        parts.push(parse(&after[..close])?);
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        parts.push(Expr::Str(rest.to_string()));
    }
    Ok(Expr::Template(parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Value<'static> {
        Value::map([
            ("n", Value::from(3)),
            ("m", Value::from(4)),
            ("name", Value::from("Alice")),
            ("flag", Value::from(true)),
            ("user", Value::map([("age", Value::from(20)), ("admin", Value::from(false))])),
            ("items", Value::seq([Value::from(1), Value::from(2), Value::from(3)])),
        ])
    }

    fn ev(src: &str) -> String {
        eval_with(src, &HashMap::new())
    }

    fn eval_with(src: &str, messages: &HashMap<String, String>) -> String {
        let e = parse(src).unwrap_or_else(|err| panic!("parse `{src}`: {err}"));
        let m = model();
        let anon = kernway_core::security::Anonymous;
        let env = Env { model: &m, messages, context_path: "", authz: &anon, csrf: None };
        let mut out = String::new();
        eval(&e, &env, &[]).write_string(&mut out);
        out
    }

    fn ev_bool(src: &str) -> bool {
        let e = parse(src).unwrap();
        let m = model();
        let messages = HashMap::new();
        let anon = kernway_core::security::Anonymous;
        let env = Env { model: &m, messages: &messages, context_path: "", authz: &anon, csrf: None };
        eval(&e, &env, &[]).to_bool()
    }

    #[test]
    fn variables_and_literals() {
        assert_eq!(ev("${name}"), "Alice");
        assert_eq!(ev("${user.age}"), "20");
        assert_eq!(ev("'hi'"), "hi");
        assert_eq!(ev("42"), "42");
        assert_eq!(ev("3.5"), "3.5");
    }

    #[test]
    fn arithmetic() {
        assert_eq!(ev("${n} + ${m}"), "7");
        assert_eq!(ev("${m} - ${n}"), "1");
        assert_eq!(ev("${n} * ${m}"), "12");
        assert_eq!(ev("${m} / 2"), "2");
        assert_eq!(ev("${m} % ${n}"), "1");
        assert_eq!(ev("-${n}"), "-3");
    }

    #[test]
    fn precedence_and_parentheses() {
        assert_eq!(ev("1 + 2 * 3"), "7");
        assert_eq!(ev("(1 + 2) * 3"), "9");
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(ev("'Hello, ' + ${name}"), "Hello, Alice");
    }

    #[test]
    fn comparison() {
        assert!(ev_bool("${user.age} >= 18"));
        assert!(ev_bool("${n} < ${m}"));
        assert!(!ev_bool("${n} > ${m}"));
        assert!(ev_bool("${n} lt ${m}")); // textual operator
    }

    #[test]
    fn equality() {
        assert!(ev_bool("${n} == 3"));
        assert!(ev_bool("${name} == 'Alice'"));
        assert!(ev_bool("${name} != 'Bob'"));
        assert!(ev_bool("${missing} == null"));
    }

    #[test]
    fn boolean_logic() {
        assert!(ev_bool("${flag} and ${user.age} > 18"));
        assert!(!ev_bool("${flag} and ${user.admin}"));
        assert!(ev_bool("${flag} or ${user.admin}"));
        assert!(ev_bool("not ${user.admin}"));
        assert!(ev_bool("!${user.admin}"));
    }

    #[test]
    fn ternary_and_elvis() {
        assert_eq!(ev("${user.age} >= 18 ? 'adult' : 'minor'"), "adult");
        assert_eq!(ev("${user.admin} ? 'yes' : 'no'"), "no");
        assert_eq!(ev("${name} ?: 'anon'"), "Alice");
        assert_eq!(ev("${missing} ?: 'anon'"), "anon");
    }

    #[test]
    fn literal_substitution() {
        assert_eq!(ev("|Hello ${name}, you are ${user.age}|"), "Hello Alice, you are 20");
    }

    #[test]
    fn whole_number_math_has_no_trailing_zero() {
        assert_eq!(ev("${n} + 1"), "4");
        assert_eq!(ev("10 / 4"), "2.5");
    }

    // --- @{…} URLs (slice C) ----------------------------------------------

    #[test]
    fn plain_url_passes_through() {
        assert_eq!(ev("@{/home}"), "/home");
    }

    #[test]
    fn url_path_variable_is_substituted() {
        // ${n} = 3
        assert_eq!(ev("@{/users/{id}(id=${n})}"), "/users/3");
    }

    #[test]
    fn url_query_params_are_appended_and_encoded() {
        assert_eq!(ev("@{/search(q='a b&c', page=2)}"), "/search?q=a%20b%26c&page=2");
    }

    #[test]
    fn url_mixes_path_and_query() {
        assert_eq!(ev("@{/u/{id}/posts(id=${n}, sort='new')}"), "/u/3/posts?sort=new");
    }

    // --- #{…} messages (slice C) ------------------------------------------

    #[test]
    fn message_lookup_with_arguments() {
        let mut msgs = HashMap::new();
        msgs.insert("greeting".to_string(), "Hello, {0}! You have {1}.".to_string());
        assert_eq!(eval_with("#{greeting(${name}, '2 msgs')}", &msgs), "Hello, Alice! You have 2 msgs.");
    }

    #[test]
    fn a_missing_message_is_visible() {
        assert_eq!(ev("#{no.such.key}"), "??no.such.key??");
    }

    // --- #utility objects (slice D) ---------------------------------------

    #[test]
    fn string_utilities() {
        assert_eq!(ev("#strings.toUpperCase(${name})"), "ALICE");
        assert_eq!(ev("#strings.toLowerCase(${name})"), "alice");
        assert_eq!(ev("#strings.length(${name})"), "5");
        assert_eq!(ev("#strings.capitalize('hello')"), "Hello");
        assert_eq!(ev("#strings.trim('  hi  ')"), "hi");
        assert!(ev_bool("#strings.contains(${name}, 'lic')"));
        assert_eq!(ev("#strings.defaultString(${missing}, 'anon')"), "anon");
    }

    #[test]
    fn number_utilities() {
        assert_eq!(ev("#numbers.formatDecimal(3.14159, 1, 2)"), "3.14");
        assert_eq!(ev("#numbers.formatInteger(5, 3)"), "005");
        assert_eq!(ev("#numbers.abs(-${n})"), "3");
    }

    #[test]
    fn list_utilities() {
        assert_eq!(ev("#lists.size(${items})"), "3");
        assert!(!ev_bool("#lists.isEmpty(${items})"));
        assert!(ev_bool("#lists.contains(${items}, '2')"));
        assert!(!ev_bool("#lists.contains(${items}, '9')"));
    }

    #[test]
    fn an_unknown_utility_is_null_not_a_crash() {
        assert_eq!(ev("#strings.noSuchMethod('x')"), "");
    }

    #[test]
    fn utilities_compose_with_operators() {
        // A utility result flows into the rest of the expression.
        assert_eq!(ev("#strings.length(${name}) + ${n}"), "8");
        assert_eq!(ev("#lists.size(${items}) > 2 ? 'many' : 'few'"), "many");
    }

    #[test]
    fn malformed_expressions_error() {
        assert!(parse("${}").is_err() || parse("1 +").is_err());
        assert!(parse("1 +").is_err());
        assert!(parse("(1").is_err());
        assert!(parse("'unterminated").is_err());
    }
}
