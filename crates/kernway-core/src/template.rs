//! Template engine abstraction — and the model an engine renders.
//!
//! Per [KEP-0003]. The model is a concrete, dynamic tree ([`Value`]) rather than
//! the `&dyn Any` a template engine could not do anything with: an engine can
//! read a scalar, walk a `Seq`, look up a `Map` field, and test truthiness, all
//! without knowing the caller's Rust types. It is dynamic on purpose — a template
//! compiled at *runtime* (for the <10 ms hot-reload M5 promises) cannot be typed
//! against a Rust struct at compile time.
//!
//! Values **borrow** where they can ([`std::borrow::Cow`]), so building a model
//! from data a handler already holds does not clone every string ([KEP-0001]).
//!
//! [KEP-0003]: https://github.com/tacpham/kernway/blob/main/docs/kep/0003-template-model.md
//! [KEP-0001]: https://github.com/tacpham/kernway/blob/main/docs/kep/0001-respect-rust.md

use std::borrow::Cow;

/// Template render error.
#[derive(Debug)]
pub struct TemplateError(pub String);

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "template error: {}", self.0)
    }
}

impl std::error::Error for TemplateError {}

/// A value in a template model — one of seven concrete shapes an engine renders.
///
/// Scalars (`Null`/`Bool`/`Int`/`Float`/`Str`) are what a template interpolates
/// or tests; `Seq`/`Map` are the two containers it walks. The lifetime `'a` is
/// the data the model borrows from — a handler builds a `Value` from its own
/// values and renders in the same call, so the borrow always outlives the render.
///
/// ```
/// use kernway_core::template::Value;
///
/// let model = Value::map([
///     ("title", Value::from("Users")),
///     ("count", Value::from(2)),
///     ("names", Value::seq(["Alice", "Bob"].iter().map(|n| Value::from(*n)))),
/// ]);
/// assert!(matches!(model.get("title"), Some(Value::Str(_))));
/// assert_eq!(model.get("count"), Some(&Value::Int(2)));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    /// Absent / nothing. What `Option::None` and an unknown field render as.
    Null,
    /// A boolean — for `{% if flag %}`.
    Bool(bool),
    /// An integer. `u64`/`i128` that do not fit go through [`Value::Str`].
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// Text. Plain, unescaped — escaping is the engine's job and depends on where
    /// the template puts the value (body vs attribute vs URL vs JS).
    Str(Cow<'a, str>),
    /// An ordered sequence — for `{% for x in xs %}`.
    Seq(Vec<Value<'a>>),
    /// An insertion-ordered set of named fields — for `{{ a.b }}`.
    ///
    /// A `Vec` of pairs, not a `HashMap`: a model touches a handful of fields, so
    /// a scan beats a hash and the render order stays stable (same finding as
    /// `Headers`/`Fields`).
    Map(Vec<(Cow<'a, str>, Value<'a>)>),
}

impl<'a> Value<'a> {
    /// Build a `Map` from name/value pairs, in order.
    pub fn map<I, K>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, Value<'a>)>,
        K: Into<Cow<'a, str>>,
    {
        Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Build a `Seq` from values, in order.
    pub fn seq<I>(values: I) -> Self
    where
        I: IntoIterator<Item = Value<'a>>,
    {
        Value::Seq(values.into_iter().collect())
    }

    /// Look up a field by name in a `Map`; `None` for a non-map or a missing key.
    ///
    /// Reverse scan, so a field written twice resolves to the last one — matching
    /// what building a map by inserting in order would hold.
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self {
            Value::Map(pairs) => pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The text of a `Str`, borrowed; `None` for any other shape.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Whether this value is *truthy* — the rule for `{% if %}`.
    ///
    /// Falsey: `Null`, `false`, `0`, `0.0`, an empty string, an empty `Seq`, an
    /// empty `Map`. Everything else is truthy. This is the model's decision, kept
    /// here so every engine agrees on it rather than each inventing its own.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Seq(xs) => !xs.is_empty(),
            Value::Map(kv) => !kv.is_empty(),
        }
    }
}

// --- From, for the scalar-literal ergonomics `Value::from(x)` / `.into()` ---

impl From<bool> for Value<'_> {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(s: &'a str) -> Self {
        Value::Str(Cow::Borrowed(s))
    }
}

impl From<String> for Value<'_> {
    fn from(s: String) -> Self {
        Value::Str(Cow::Owned(s))
    }
}

impl<'a> From<Cow<'a, str>> for Value<'a> {
    fn from(s: Cow<'a, str>) -> Self {
        Value::Str(s)
    }
}

impl From<f64> for Value<'_> {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value<'_> {
            fn from(n: $t) -> Self { Value::Int(n as i64) }
        }
    )*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32);

/// Convert a Rust value into a template [`Value`], borrowing from `self`.
///
/// Implemented for the scalars, `Option<T>` (`None` → `Null`), and slices/`Vec`
/// (→ `Seq`). A struct becomes a `Value::Map` — by hand today, by a
/// `#[derive(Model)]` later (KEP-0003, Future possibilities).
///
/// Deliberately **not** `serde::Serialize`: `kernway-core` carries no serde
/// ([KEP-0000 §1]). The model is a handful of variants we own.
///
/// [KEP-0000 §1]: https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md
pub trait ToValue {
    /// This value as a template [`Value`], borrowing where it can.
    fn to_value(&self) -> Value<'_>;
}

macro_rules! to_value_via_from {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Value<'_> { Value::from(*self) }
        }
    )*};
}
to_value_via_from!(bool, i8, i16, i32, i64, u8, u16, u32, f64);

impl ToValue for str {
    fn to_value(&self) -> Value<'_> {
        Value::Str(Cow::Borrowed(self))
    }
}

impl ToValue for String {
    fn to_value(&self) -> Value<'_> {
        Value::Str(Cow::Borrowed(self.as_str()))
    }
}

impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value<'_> {
        match self {
            Some(v) => v.to_value(),
            None => Value::Null,
        }
    }
}

impl<T: ToValue> ToValue for [T] {
    fn to_value(&self) -> Value<'_> {
        Value::Seq(self.iter().map(ToValue::to_value).collect())
    }
}

impl<T: ToValue> ToValue for Vec<T> {
    fn to_value(&self) -> Value<'_> {
        self.as_slice().to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value<'_> {
        (**self).to_value()
    }
}

/// Template engine — renders a named template against a [`Value`] model into HTML.
///
/// Equivalent to `ViewResolver` + `TemplateEngine` in Spring MVC/Thymeleaf.
/// `KernleafEngine` will implement this trait.
///
/// `render` is synchronous and returns a `String`: parsing and IR compilation
/// happen inside the engine at load/watch time, never on the request path
/// ([KEP-0000 §4]). Implementations HTML-escape interpolated values by default,
/// context-aware, so a template cannot become an XSS vector by accident.
///
/// [KEP-0000 §4]: https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md
pub trait TemplateEngine: Send + Sync {
    /// Render `template` (a name the engine resolves) against `model`.
    fn render(&self, template: &str, model: &Value<'_>) -> Result<String, TemplateError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the model ---------------------------------------------------------

    #[test]
    fn map_lookup_and_missing_key() {
        let m = Value::map([("a", Value::from(1)), ("b", Value::from("two"))]);
        assert_eq!(m.get("a"), Some(&Value::Int(1)));
        assert_eq!(m.get("b").and_then(Value::as_str), Some("two"));
        assert_eq!(m.get("missing"), None);
    }

    #[test]
    fn a_repeated_field_resolves_to_the_last() {
        let m = Value::map([("x", Value::from(1)), ("x", Value::from(2))]);
        assert_eq!(m.get("x"), Some(&Value::Int(2)));
    }

    #[test]
    fn truthiness_rules() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(!Value::Int(0).is_truthy());
        assert!(!Value::from("").is_truthy());
        assert!(!Value::seq([]).is_truthy());
        assert!(Value::Bool(true).is_truthy());
        assert!(Value::Int(1).is_truthy());
        assert!(Value::from("x").is_truthy());
        assert!(Value::seq([Value::from(1)]).is_truthy());
    }

    #[test]
    fn to_value_borrows_strings_rather_than_cloning() {
        let name = String::from("Alice");
        let v = name.to_value();
        // Borrowed, not owned — no allocation was made to build the model.
        assert!(matches!(v, Value::Str(Cow::Borrowed(_))));
    }

    #[test]
    fn to_value_for_option_and_seq() {
        let some: Option<i32> = Some(7);
        let none: Option<i32> = None;
        assert_eq!(some.to_value(), Value::Int(7));
        assert_eq!(none.to_value(), Value::Null);

        let xs = vec![1i32, 2, 3];
        assert_eq!(
            xs.to_value(),
            Value::Seq(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
    }

    // --- proof the trait is implementable ----------------------------------
    //
    // The whole point of KEP-0003: an engine can be written against `Value`
    // with no `downcast` anywhere. This minimal engine interpolates `{{ key }}`
    // and expands `{{#each items}}…{{.}}…{{/each}}`, purely by walking the model.
    // If it compiles and renders, the model is implementable — which the old
    // `&dyn Any` trait was not.

    struct ToyEngine;

    impl ToyEngine {
        fn escape(s: &str) -> String {
            s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        }
    }

    impl TemplateEngine for ToyEngine {
        fn render(&self, template: &str, model: &Value<'_>) -> Result<String, TemplateError> {
            // {{#each key}}BODY{{/each}} — repeat BODY for each item, `{{.}}` = item.
            if let (Some(open), Some(close)) = (template.find("{{#each "), template.find("{{/each}}")) {
                let key_end = template[open..].find("}}").ok_or_else(|| TemplateError("bad each".into()))? + open;
                let key = template[open + 8..key_end].trim();
                let body = &template[key_end + 2..close];
                let items = match model.get(key) {
                    Some(Value::Seq(xs)) => xs,
                    _ => return Err(TemplateError(format!("`{key}` is not a sequence"))),
                };
                let mut out = String::new();
                for item in items {
                    let text = item.as_str().unwrap_or("");
                    out.push_str(&body.replace("{{.}}", &Self::escape(text)));
                }
                return Ok(out);
            }
            // {{ key }} — interpolate a single field, escaped.
            let mut out = template.to_string();
            while let (Some(open), Some(close)) = (out.find("{{"), out.find("}}")) {
                let key = out[open + 2..close].trim().to_string();
                let value = model.get(&key).and_then(Value::as_str).unwrap_or("");
                out.replace_range(open..close + 2, &Self::escape(value));
            }
            Ok(out)
        }
    }

    #[test]
    fn a_reference_engine_interpolates_against_the_model() {
        let model = Value::map([("name", Value::from("Alice"))]);
        assert_eq!(ToyEngine.render("Hi {{ name }}!", &model).unwrap(), "Hi Alice!");
    }

    #[test]
    fn the_reference_engine_escapes_by_default() {
        // The XSS case M4's gate cares about: an interpolated value is escaped.
        let model = Value::map([("q", Value::from("<script>alert(1)</script>"))]);
        let out = ToyEngine.render("You searched: {{ q }}", &model).unwrap();
        assert_eq!(out, "You searched: &lt;script&gt;alert(1)&lt;/script&gt;");
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn the_reference_engine_iterates_a_seq() {
        let model = Value::map([(
            "items",
            Value::seq(["a", "b", "c"].iter().map(|s| Value::from(*s))),
        )]);
        let out = ToyEngine.render("{{#each items}}[{{.}}]{{/each}}", &model).unwrap();
        assert_eq!(out, "[a][b][c]");
    }
}
