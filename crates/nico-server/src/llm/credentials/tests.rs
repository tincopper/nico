//! What a credential reference reads, and what a failure is allowed to say.
//!
//! The environment's success path is exercised through a variable the test runner already exports:
//! mutating the environment is `unsafe` in the 2024 edition and shared with every other test in the
//! binary, so an `env:` value this file creates could not be trusted even if it could be written. The
//! causes that need an empty or non-UTF-8 *value* are therefore covered through `file:`, which
//! exercises the same three checks with bytes this test owns.

use std::fs;

use nico_core::ErrorCategory;

use super::{
    CredentialResolver, EMPTY, EnvFileCredentials, MAX_CREDENTIAL_BYTES, NO_NAME, NO_SOURCE,
    NOT_SET, NOT_UTF8, TOO_LARGE, UNREADABLE, check_value,
};

/// The secret every file fixture holds.
const SECRET: &str = "sk-fixture-credential";

/// Returns the resolver under test.
fn resolver() -> EnvFileCredentials {
    EnvFileCredentials::new()
}

/// Writes `bytes` to `name` in a fresh temporary directory and returns its `file:` reference.
fn file_fixture(bytes: &[u8]) -> (tempfile::TempDir, String) {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("credential");
    fs::write(&path, bytes).expect("the credential fixture is written");
    let reference = format!("file:{}", path.display());
    (directory, reference)
}

#[test]
fn an_environment_variable_is_read_as_written() {
    let expected = std::env::var("PATH").expect("the test runner exports PATH");
    let credential = resolver()
        .resolve("env:PATH")
        .expect("an exported variable resolves");

    assert_eq!(
        credential.expose(),
        expected,
        "the value is the variable's, untouched"
    );
}

#[test]
fn a_missing_variable_is_an_upstream_failure_that_names_the_reference() {
    let error = resolver()
        .resolve("env:NICO_TEST_VARIABLE_THAT_IS_NEVER_SET")
        .expect_err("an unset variable is refused");

    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(error.cause(), NOT_SET);
    assert_eq!(
        error.reference(),
        "env:NICO_TEST_VARIABLE_THAT_IS_NEVER_SET"
    );
    assert!(
        error
            .to_string()
            .contains("env:NICO_TEST_VARIABLE_THAT_IS_NEVER_SET"),
        "an operator must be able to find the entry: {error}"
    );
}

#[test]
fn a_reference_without_a_known_scheme_is_the_documents_mistake() {
    let error = resolver()
        .resolve("OPENAI_API_KEY")
        .expect_err("a bare name is not a reference");

    assert_eq!(
        error.category(),
        ErrorCategory::Client,
        "the document wrote something this build does not read, which is not a dependency failure"
    );
    assert_eq!(error.cause(), NO_SOURCE);
}

#[test]
fn a_reference_that_names_nothing_is_refused() {
    for reference in ["env:", "file:"] {
        let error = resolver()
            .resolve(reference)
            .expect_err("an empty name reads nothing");
        assert_eq!(error.cause(), NO_NAME, "for `{reference}`");
        assert_eq!(error.category(), ErrorCategory::Client);
        assert_eq!(error.reference(), reference);
    }
}

#[test]
fn a_file_is_read_byte_for_byte_including_a_trailing_newline() {
    let (_directory, reference) = file_fixture(format!("{SECRET}\n").as_bytes());
    let credential = resolver()
        .resolve(&reference)
        .expect("a readable file resolves");

    assert_eq!(
        credential.expose(),
        format!("{SECRET}\n"),
        "`echo secret > file` writes a different key, and this side must not repair it silently"
    );
}

#[test]
fn neither_source_accepts_a_value_this_build_would_not_send() {
    // An `env:` value cannot be created from a test in the 2024 edition, so the rule those bytes
    // would meet is asserted directly. It is the same function both sources call, which is what stops
    // a bound applied to one branch from being forgotten by the other.
    let error = check_value("env:FIXTURE", "").expect_err("an empty value is unusable");
    assert_eq!(error.cause(), EMPTY);
    assert_eq!(error.category(), ErrorCategory::Upstream);

    let at_bound = "k".repeat(MAX_CREDENTIAL_BYTES);
    check_value("env:FIXTURE", &at_bound).expect("the bound is inclusive");

    let error = check_value("env:FIXTURE", &format!("{at_bound}k"))
        .expect_err("one byte over the bound is refused");
    assert_eq!(error.cause(), TOO_LARGE);
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(
        error.reference(),
        "env:FIXTURE",
        "the failure names the reference, never the value"
    );
}

#[test]
fn a_file_at_the_bound_is_accepted_and_one_byte_over_is_refused() {
    let (_directory, reference) = file_fixture(&vec![b'k'; MAX_CREDENTIAL_BYTES]);
    resolver()
        .resolve(&reference)
        .expect("the bound is inclusive");

    let (_directory, reference) = file_fixture(&vec![b'k'; MAX_CREDENTIAL_BYTES + 1]);
    let error = resolver()
        .resolve(&reference)
        .expect_err("a value over the bound is refused");
    assert_eq!(error.cause(), TOO_LARGE);
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn a_missing_file_is_unreadable_and_quotes_no_io_error() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let reference = format!("file:{}", directory.path().join("absent").display());
    let error = resolver().resolve(&reference).expect_err("no such file");

    assert_eq!(error.cause(), UNREADABLE);
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert!(
        !error.to_string().contains("No such file"),
        "the cause is a fixed sentence this crate wrote: {error}"
    );
}

#[test]
fn an_empty_file_is_refused() {
    let (_directory, reference) = file_fixture(b"");
    let error = resolver().resolve(&reference).expect_err("an empty key");

    assert_eq!(error.cause(), EMPTY);
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn a_value_that_is_not_utf8_is_refused_without_quoting_it() {
    let (_directory, reference) = file_fixture(b"sk-\xff\xfe-do-not-print");
    let error = resolver().resolve(&reference).expect_err("not UTF-8");

    assert_eq!(error.cause(), NOT_UTF8);
    let rendered = error.to_string();
    assert!(
        !rendered.contains("do-not-print"),
        "a refusal never carries the bytes it read: {rendered}"
    );
}
