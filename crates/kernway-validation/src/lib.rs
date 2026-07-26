//! # kernway-validation
//!
//! Request-input validation — the web-layer concern of "is this input well-formed",
//! independent of any database (KEP-0000 §1). A [`Validate`] type checks its fields
//! against constraints and collects **every** failure (not fail-fast), so a client
//! gets the whole list at once. The `#[derive(Validate)]` macro generates the checks
//! from field attributes; the `Validated<T>` extractor (in `kernway-web`) runs them
//! and renders an RFC 7807 `400` on failure.
//!
//! ```
//! use kernway_validation::{rules, Validate, ValidationErrors};
//!
//! struct CreateUser { name: String, email: String, age: u8 }
//!
//! impl Validate for CreateUser {
//!     fn validate(&self) -> Result<(), ValidationErrors> {
//!         let mut errors = ValidationErrors::new();
//!         if let Err(m) = rules::not_blank(&self.name) { errors.push("name", m); }
//!         if let Err(m) = rules::length(&self.name, Some(3), Some(50)) { errors.push("name", m); }
//!         if let Err(m) = rules::email(&self.email) { errors.push("email", m); }
//!         if let Err(m) = rules::range(self.age, Some(0), Some(150)) { errors.push("age", m); }
//!         errors.into_result()
//!     }
//! }
//! ```
//!
//! `#[derive(Validate)]` generates exactly that from `#[validate(not_blank,
//! length(min = 3, max = 50))]` field attributes.

#![forbid(unsafe_code)]

// Let `#[derive(Validate)]`-generated `::kernway_validation::…` paths resolve in
// this crate's own tests.
extern crate self as kernway_validation;

use std::fmt;

/// A validation type: checks its fields and returns every failure at once.
pub trait Validate {
    /// Validate `self`; `Ok(())` if every constraint holds, else all the failures.
    fn validate(&self) -> Result<(), ValidationErrors>;
}

/// One field's failure: which field, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// The field that failed (the struct field name).
    pub field: String,
    /// A human-readable message for this failure.
    pub message: String,
}

/// Every field failure from one [`Validate::validate`], accumulated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors {
    errors: Vec<FieldError>,
}

impl ValidationErrors {
    /// An empty error set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a failure for `field`.
    pub fn push(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.errors.push(FieldError { field: field.into(), message: message.into() });
    }

    /// Whether there are no failures.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// The failures.
    #[must_use]
    pub fn errors(&self) -> &[FieldError] {
        &self.errors
    }

    /// `Ok(())` if empty, else `Err(self)` — the tail of a generated `validate`.
    ///
    /// # Errors
    /// Returns the collected [`ValidationErrors`] when any field failed.
    pub fn into_result(self) -> Result<(), ValidationErrors> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "validation failed: ")?;
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{}: {}", error.field, error.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// The constraint checks the derive calls — each returns `Ok(())` or an error
/// message. Usable directly for a hand-written [`Validate`] too.
pub mod rules {
    /// Non-blank after trimming (rejects `""` and all-whitespace).
    ///
    /// # Errors
    /// Returns a message when the value is blank.
    pub fn not_blank(value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            Err("must not be blank".to_string())
        } else {
            Ok(())
        }
    }

    /// Character length within `[min, max]` (either bound optional).
    ///
    /// # Errors
    /// Returns a message when the length is outside the bounds.
    pub fn length(value: &str, min: Option<usize>, max: Option<usize>) -> Result<(), String> {
        let len = value.chars().count();
        if let Some(min) = min {
            if len < min {
                return Err(format!("must be at least {min} characters"));
            }
        }
        if let Some(max) = max {
            if len > max {
                return Err(format!("must be at most {max} characters"));
            }
        }
        Ok(())
    }

    /// A basic, dependency-free email shape: one `@`, a non-empty local part, and a
    /// dotted domain. Not full RFC 5322 (that needs a regex) — it catches the obvious
    /// mistakes, which is what input validation is for.
    ///
    /// # Errors
    /// Returns a message when the value is not a plausible email address.
    pub fn email(value: &str) -> Result<(), String> {
        let plausible = match value.split_once('@') {
            Some((local, domain)) => {
                value.matches('@').count() == 1
                    && !local.is_empty()
                    && domain.len() >= 3
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
            }
            None => false,
        };
        if plausible {
            Ok(())
        } else {
            Err("must be a valid email address".to_string())
        }
    }

    /// A numeric value within `[min, max]` (either bound optional). For numeric
    /// fields (`Copy`); the derive passes the field by value.
    ///
    /// # Errors
    /// Returns a message when the value is outside the bounds.
    pub fn range<T>(value: T, min: Option<T>, max: Option<T>) -> Result<(), String>
    where
        T: PartialOrd + std::fmt::Display + Copy,
    {
        if let Some(min) = min {
            if value < min {
                return Err(format!("must be at least {min}"));
            }
        }
        if let Some(max) = max {
            if value > max {
                return Err(format!("must be at most {max}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use di_macro::Validate as DeriveValidate;

    #[test]
    fn rules_pass_and_fail_as_expected() {
        assert!(rules::not_blank("hi").is_ok());
        assert!(rules::not_blank("   ").is_err());
        assert!(rules::length("abc", Some(3), Some(5)).is_ok());
        assert!(rules::length("ab", Some(3), None).is_err());
        assert!(rules::length("abcdef", None, Some(5)).is_err());
        assert!(rules::email("a@b.com").is_ok());
        assert!(rules::email("nope").is_err());
        assert!(rules::email("a@b").is_err());
        assert!(rules::range(5u8, Some(0), Some(10)).is_ok());
        assert!(rules::range(11u8, Some(0), Some(10)).is_err());
        assert!(rules::range(-1i32, Some(0), None).is_err());
    }

    #[test]
    fn errors_accumulate_all_failures() {
        let mut errors = ValidationErrors::new();
        errors.push("name", "must not be blank");
        errors.push("age", "must be at least 0");
        assert_eq!(errors.errors().len(), 2);
        assert!(errors.into_result().is_err());
        assert!(ValidationErrors::new().into_result().is_ok());
    }

    #[derive(DeriveValidate)]
    struct CreateUser {
        #[validate(not_blank, length(min = 3, max = 50))]
        name: String,
        #[validate(email)]
        email: String,
        #[validate(range(min = 0, max = 150))]
        age: u8,
        // No #[validate] → not checked.
        #[allow(dead_code)]
        note: String,
    }

    #[test]
    fn derive_validates_every_field_and_collects_all() {
        let ok = CreateUser {
            name: "Alice".into(),
            email: "alice@example.com".into(),
            age: 30,
            note: String::new(),
        };
        assert!(ok.validate().is_ok());

        let bad = CreateUser {
            name: "Al".into(),          // too short
            email: "not-an-email".into(), // bad email
            age: 200,                    // out of range
            note: String::new(),
        };
        let errors = bad.validate().unwrap_err();
        let fields: Vec<&str> = errors.errors().iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"name"), "name failed: {errors}");
        assert!(fields.contains(&"email"), "email failed: {errors}");
        assert!(fields.contains(&"age"), "age failed: {errors}");
    }
}
