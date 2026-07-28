//! Composable query predicates — the Specification pattern.
//!
//! [`Spec`] is a first-class **predicate tree** you build up and combine with
//! [`and`](Spec::and) / [`or`](Spec::or) / [`not`](Spec::not), then hand to a
//! repository. Unlike the fluent [`QueryBuilder`](crate::query::QueryBuilder) —
//! whose filters are always `AND`-ed — a `Spec` expresses arbitrary boolean
//! trees, so `OR` and nesting are first-class:
//!
//! ```
//! use kernway_orm_core::spec::Spec;
//!
//! // role = ADMIN AND (age > 18 OR vip = true)
//! let spec = Spec::eq("role", "ADMIN")
//!     .and(Spec::gt("age", "18").or(Spec::eq("vip", "true")));
//! # let _ = spec;
//! ```
//!
//! Each backend translates the tree its own way (SQL `WHERE`, a Meilisearch
//! filter expression, an in-memory walk). This module owns the tree and a
//! generic [`matches`](Spec::matches) evaluator for in-memory backends; the
//! string-rendering translations live in the individual drivers.

use std::cmp::Ordering;

/// A composable query predicate. Leaf variants mirror the
/// [`QueryBuilder`](crate::query::QueryBuilder) filters; the tail three combine
/// them into a boolean tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// `field = value`
    Eq(&'static str, String),
    /// `field != value`
    Ne(&'static str, String),
    /// `field > value`
    Gt(&'static str, String),
    /// `field < value`
    Lt(&'static str, String),
    /// `field >= value`
    Gte(&'static str, String),
    /// `field <= value`
    Lte(&'static str, String),
    /// Substring / full-text match on `field`.
    Like(&'static str, String),
    /// `field` is one of the values.
    In(&'static str, Vec<String>),
    /// `from <= field <= to`
    Between(&'static str, String, String),
    /// `field IS NULL` (absent or empty).
    IsNull(&'static str),
    /// `field IS NOT NULL` (present and non-empty).
    IsNotNull(&'static str),
    /// Both sub-predicates hold.
    And(Box<Spec>, Box<Spec>),
    /// Either sub-predicate holds.
    Or(Box<Spec>, Box<Spec>),
    /// The sub-predicate does not hold.
    Not(Box<Spec>),
}

impl Spec {
    /// `field = value`.
    pub fn eq(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Eq(field, value.into())
    }
    /// `field != value`.
    pub fn ne(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Ne(field, value.into())
    }
    /// `field > value`.
    pub fn gt(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Gt(field, value.into())
    }
    /// `field < value`.
    pub fn lt(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Lt(field, value.into())
    }
    /// `field >= value`.
    pub fn gte(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Gte(field, value.into())
    }
    /// `field <= value`.
    pub fn lte(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Lte(field, value.into())
    }
    /// Substring / full-text match.
    pub fn like(field: &'static str, value: impl Into<String>) -> Self {
        Spec::Like(field, value.into())
    }
    /// `field` in the given set.
    pub fn in_(field: &'static str, values: impl IntoIterator<Item = String>) -> Self {
        Spec::In(field, values.into_iter().collect())
    }
    /// `from <= field <= to`.
    pub fn between(field: &'static str, from: impl Into<String>, to: impl Into<String>) -> Self {
        Spec::Between(field, from.into(), to.into())
    }
    /// `field IS NULL`.
    pub fn is_null(field: &'static str) -> Self {
        Spec::IsNull(field)
    }
    /// `field IS NOT NULL`.
    pub fn is_not_null(field: &'static str) -> Self {
        Spec::IsNotNull(field)
    }

    /// Combine with `AND`.
    #[must_use]
    pub fn and(self, other: Spec) -> Self {
        Spec::And(Box::new(self), Box::new(other))
    }
    /// Combine with `OR`.
    #[must_use]
    pub fn or(self, other: Spec) -> Self {
        Spec::Or(Box::new(self), Box::new(other))
    }
    /// Negate. (Reads as `spec.not()`; not the `std::ops::Not` trait.)
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Self {
        Spec::Not(Box::new(self))
    }

    /// Evaluate the predicate against one row, given a `field -> value` accessor
    /// (returns `None` for an absent field). Numeric-looking values compare
    /// numerically, everything else lexicographically — the same rule the
    /// stringly-typed `QueryBuilder` filters use.
    ///
    /// This is the path in-memory backends use; SQL / search backends render the
    /// tree to their own query language instead.
    pub fn matches<F>(&self, get: &F) -> bool
    where
        F: Fn(&str) -> Option<String>,
    {
        match self {
            Spec::Eq(f, v) => cmp(get(f), v) == Some(Ordering::Equal),
            Spec::Ne(f, v) => matches!(cmp(get(f), v), Some(o) if o != Ordering::Equal),
            Spec::Gt(f, v) => cmp(get(f), v) == Some(Ordering::Greater),
            Spec::Lt(f, v) => cmp(get(f), v) == Some(Ordering::Less),
            Spec::Gte(f, v) => matches!(cmp(get(f), v), Some(Ordering::Greater | Ordering::Equal)),
            Spec::Lte(f, v) => matches!(cmp(get(f), v), Some(Ordering::Less | Ordering::Equal)),
            Spec::Like(f, v) => get(f).is_some_and(|c| c.contains(v.as_str())),
            Spec::In(f, vs) => {
                let got = get(f);
                vs.iter().any(|v| cmp(got.clone(), v) == Some(Ordering::Equal))
            }
            Spec::Between(f, lo, hi) => {
                let got = get(f);
                matches!(cmp(got.clone(), lo), Some(Ordering::Greater | Ordering::Equal))
                    && matches!(cmp(got, hi), Some(Ordering::Less | Ordering::Equal))
            }
            // (Not `is_none_or` — that is Rust 1.82+, and the MSRV is 1.78.)
            Spec::IsNull(f) => match get(f) {
                None => true,
                Some(c) => c.is_empty(),
            },
            Spec::IsNotNull(f) => get(f).is_some_and(|c| !c.is_empty()),
            Spec::And(a, b) => a.matches(get) && b.matches(get),
            Spec::Or(a, b) => a.matches(get) || b.matches(get),
            Spec::Not(s) => !s.matches(get),
        }
    }
}

/// Compare an optional field value to a query value: numeric when both parse as
/// numbers, lexicographic otherwise. `None` (absent field) never compares.
fn cmp(field: Option<String>, query: &str) -> Option<Ordering> {
    let field = field?;
    match (field.parse::<f64>(), query.parse::<f64>()) {
        (Ok(a), Ok(b)) => a.partial_cmp(&b),
        _ => Some(field.as_str().cmp(query)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A row accessor over a small owned map.
    fn row(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |f| map.get(f).cloned()
    }

    #[test]
    fn eq_and_ne() {
        let r = row(&[("role", "ADMIN")]);
        assert!(Spec::eq("role", "ADMIN").matches(&r));
        assert!(!Spec::eq("role", "USER").matches(&r));
        assert!(Spec::ne("role", "USER").matches(&r));
        assert!(!Spec::ne("role", "ADMIN").matches(&r));
    }

    #[test]
    fn numeric_comparisons_are_numeric_not_lexicographic() {
        let r = row(&[("age", "9")]);
        // Lexicographically "9" > "18"; numerically 9 < 18.
        assert!(Spec::lt("age", "18").matches(&r));
        assert!(!Spec::gt("age", "18").matches(&r));
        assert!(Spec::gte("age", "9").matches(&r));
    }

    #[test]
    fn like_in_between_null() {
        let r = row(&[("name", "Electric Drill"), ("price", "25"), ("note", "")]);
        assert!(Spec::like("name", "Drill").matches(&r));
        assert!(!Spec::like("name", "Saw").matches(&r));
        assert!(Spec::in_("price", ["10".into(), "25".into()]).matches(&r));
        assert!(Spec::between("price", "10", "30").matches(&r));
        assert!(!Spec::between("price", "30", "40").matches(&r));
        assert!(Spec::is_null("note").matches(&r)); // empty counts as null
        assert!(Spec::is_null("missing").matches(&r)); // absent counts as null
        assert!(Spec::is_not_null("name").matches(&r));
        assert!(!Spec::is_not_null("note").matches(&r));
    }

    #[test]
    fn and_or_not_truth_tables() {
        let r = row(&[("role", "ADMIN"), ("age", "20"), ("vip", "false")]);

        // role = ADMIN AND (age > 18 OR vip = true)
        let spec = Spec::eq("role", "ADMIN").and(Spec::gt("age", "18").or(Spec::eq("vip", "true")));
        assert!(spec.matches(&r));

        // Under-18 non-vip admin fails the OR branch.
        let young = row(&[("role", "ADMIN"), ("age", "15"), ("vip", "false")]);
        assert!(!spec.matches(&young));

        // NOT flips it.
        assert!(!Spec::eq("role", "ADMIN").not().matches(&r));
        assert!(Spec::eq("role", "USER").not().matches(&r));

        // OR short-circuits truthiness.
        assert!(Spec::eq("role", "USER").or(Spec::eq("role", "ADMIN")).matches(&r));
    }

    #[test]
    fn absent_field_never_matches_a_comparison() {
        let r = row(&[("a", "1")]);
        assert!(!Spec::eq("missing", "x").matches(&r));
        assert!(!Spec::gt("missing", "0").matches(&r));
        assert!(!Spec::ne("missing", "x").matches(&r)); // absent is not "!= x"
    }

    #[test]
    fn combinators_build_the_expected_tree() {
        let s = Spec::eq("a", "1").and(Spec::eq("b", "2"));
        assert_eq!(
            s,
            Spec::And(
                Box::new(Spec::Eq("a", "1".into())),
                Box::new(Spec::Eq("b", "2".into()))
            )
        );
    }
}
