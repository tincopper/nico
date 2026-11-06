//! What the bounded reader accepts and what it refuses.

use std::fs;

use super::{ReadError, read_bounded};

#[test]
fn a_regular_file_at_the_bound_is_read_and_one_byte_over_is_refused() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("document");

    fs::write(&path, vec![b'k'; 64]).expect("the fixture is written");
    assert_eq!(
        read_bounded(&path, 64).expect("the bound is inclusive"),
        vec![b'k'; 64]
    );

    fs::write(&path, vec![b'k'; 65]).expect("the fixture is written");
    let error = read_bounded(&path, 64).expect_err("one byte over is refused");
    match error {
        ReadError::TooLarge { at_least, limit } => {
            assert_eq!(limit, 64);
            assert_eq!(
                at_least, 65,
                "the reader stops one past the ceiling, so the size is known only as a lower bound"
            );
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn a_directory_is_not_a_regular_file() {
    // The check exists because a length is only meaningful for a regular file: a FIFO reports zero and
    // a device reports whatever it likes, so a bound derived from the length would be a fiction.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let error = read_bounded(directory.path(), 4096).expect_err("a directory is not a document");
    assert!(matches!(error, ReadError::NotRegular), "got {error:?}");
}

#[test]
fn a_missing_file_is_an_io_failure_that_names_no_path() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let error = read_bounded(&directory.path().join("absent"), 4096)
        .expect_err("an absent file cannot be read");
    assert!(matches!(error, ReadError::Io(_)), "got {error:?}");
    assert_eq!(
        error.to_string(),
        "the file cannot be read",
        "the display names the failure, never the path it names"
    );
}
