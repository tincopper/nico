//! The password reference reader: the same conventions the config source keeps.

use super::read_password_ref;

#[test]
fn a_missing_variable_names_the_fact_without_the_value() {
    let error = read_password_ref("env:NICO_QUOTA_TEST_UNSET").expect_err("unset is refused");
    assert!(error.contains("not set"));
    assert!(
        !error.to_lowercase().contains("password"),
        "the message never names a secret"
    );
}

#[test]
fn a_file_reference_is_read_byte_for_byte() {
    let mut path = std::env::temp_dir();
    path.push("nico-quota-password-file-test");
    std::fs::write(&path, b"line\n").expect("the fixture writes");
    let value = read_password_ref(&format!("file:{}", path.display())).expect("the file is read");
    assert_eq!(
        value, "line\n",
        "the trailing newline is part of the password"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unknown_scheme_is_refused_with_a_fixed_sentence() {
    let error = read_password_ref("value:s3cret").expect_err("values are never accepted");
    assert!(error.contains("env:"));
}

#[test]
fn an_empty_file_is_refused() {
    let mut path = std::env::temp_dir();
    path.push("nico-quota-password-empty-test");
    std::fs::write(&path, b"").expect("the fixture writes");
    let error =
        read_password_ref(&format!("file:{}", path.display())).expect_err("empty is refused");
    assert!(error.contains("empty"));
    let _ = std::fs::remove_file(&path);
}
