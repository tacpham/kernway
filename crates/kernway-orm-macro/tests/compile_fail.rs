//! Misuse of the ORM macros is a compile error with a message that says what
//! is wrong, pointing at the offending code (checked against `tests/ui/*.stderr`).

#[test]
fn misuse_is_rejected_at_compile_time() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
