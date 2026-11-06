//! What the last-known-good cache stores, and what it refuses to store.
//!
//! Split from `cache.rs` where the file budget asked for it: the store's rules are declared there, and
//! these are the cases that pin them.

use super::*;

const DOC_A: &[u8] = b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 1024\n";
const DOC_B: &[u8] = b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 2048\n";

fn meta_for(bytes: &[u8], generation: u64) -> CacheMeta {
    CacheMeta::new(
        7,
        ConfigDigest::of(bytes).hex(),
        generation,
        1_700_000_000,
        "local",
    )
}

fn store() -> (tempfile::TempDir, CacheStore) {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let store = CacheStore::new(directory.path().join("cache"));
    (directory, store)
}

mod round_trip {
    use super::*;

    #[tokio::test]
    async fn write_then_read_returns_the_same_document_and_metadata() {
        let (_directory, store) = store();
        let meta = meta_for(DOC_A, 3);

        store.write(DOC_A, &meta).await.expect("write succeeds");
        let cached = store.read().await.expect("read succeeds");

        assert_eq!(cached.bytes().as_ref(), DOC_A);
        assert_eq!(cached.meta(), &meta);
        assert_eq!(cached.meta().revision_seq(), 7);
        assert_eq!(cached.meta().source(), "local");
    }

    #[tokio::test]
    async fn write_replaces_the_previous_entry() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 1))
            .await
            .expect("first write succeeds");
        store
            .write(DOC_B, &meta_for(DOC_B, 2))
            .await
            .expect("second write succeeds");

        let cached = store.read().await.expect("read succeeds");
        assert_eq!(cached.bytes().as_ref(), DOC_B);
        assert_eq!(cached.meta().generation(), 2);
    }

    #[tokio::test]
    async fn write_leaves_no_temporary_files_behind() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 1))
            .await
            .expect("write succeeds");

        let mut names: Vec<String> = std::fs::read_dir(store.directory())
            .expect("the cache directory is readable")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec![META_FILE.to_owned(), DOCUMENT_FILE.to_owned()]);
    }
}

mod validation {
    use super::*;

    #[tokio::test]
    async fn read_reports_corrupt_when_the_document_does_not_match_the_digest() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 1))
            .await
            .expect("write succeeds");
        std::fs::write(store.directory().join(DOCUMENT_FILE), DOC_B).expect("overwrite succeeds");

        assert_eq!(
            store.read().await.expect_err("the digest does not match"),
            CacheError::Corrupt {
                reason: "cached document does not match its digest"
            }
        );
    }

    #[tokio::test]
    async fn read_reports_corrupt_when_the_generation_is_zero() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 0))
            .await
            .expect("write succeeds");

        assert_eq!(
            store
                .read()
                .await
                .expect_err("generation zero is not a published snapshot"),
            CacheError::Corrupt {
                reason: "cached snapshot has no generation"
            }
        );
    }

    #[tokio::test]
    async fn read_reports_corrupt_when_the_metadata_is_not_yaml() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 1))
            .await
            .expect("write succeeds");
        std::fs::write(store.directory().join(META_FILE), b"not: [valid")
            .expect("overwrite succeeds");

        assert_eq!(
            store.read().await.expect_err("the metadata is unreadable"),
            CacheError::Corrupt {
                reason: "cache metadata is not valid YAML"
            }
        );
    }

    #[tokio::test]
    async fn read_reports_corrupt_when_the_metadata_has_unknown_fields() {
        let (_directory, store) = store();
        std::fs::create_dir_all(store.directory()).expect("create the cache directory");
        std::fs::write(
            store.directory().join(META_FILE),
            b"revision_seq: 7\ndigest_hex: 00\ngeneration: 1\nwritten_at_unix_secs: 1\nsource: local\nextra: 1\n",
        )
        .expect("write succeeds");

        assert!(matches!(
            store.read().await.expect_err("unknown fields are rejected"),
            CacheError::Corrupt { .. }
        ));
    }

    #[tokio::test]
    async fn read_reports_io_when_no_cache_exists() {
        let (_directory, store) = store();
        let error = store.read().await.expect_err("nothing is cached yet");
        match error {
            CacheError::Io { source } => assert_eq!(source.kind(), io::ErrorKind::NotFound),
            other => panic!("expected an Io error, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_maps_to_internal_and_io_to_upstream() {
        assert_eq!(
            CacheError::Corrupt { reason: "x" }.category(),
            ErrorCategory::Internal
        );
        assert_eq!(
            CacheError::Io {
                source: io::Error::other("x")
            }
            .category(),
            ErrorCategory::Upstream
        );
    }

    #[test]
    fn io_errors_compare_by_kind_and_message() {
        assert_eq!(
            CacheError::Io {
                source: io::Error::new(io::ErrorKind::PermissionDenied, "nope")
            },
            CacheError::Io {
                source: io::Error::new(io::ErrorKind::PermissionDenied, "nope")
            }
        );
        assert_ne!(
            CacheError::Io {
                source: io::Error::new(io::ErrorKind::PermissionDenied, "nope")
            },
            CacheError::Corrupt { reason: "nope" }
        );
    }
}

mod failures {
    use super::*;

    #[tokio::test]
    async fn write_leaves_the_previous_entry_intact_when_the_document_cannot_be_written() {
        let (_directory, store) = store();
        store
            .write(DOC_A, &meta_for(DOC_A, 1))
            .await
            .expect("first write succeeds");
        let previous_meta =
            std::fs::read(store.directory().join(META_FILE)).expect("meta is readable");

        // Occupying the temporary document path with a directory makes the first write step fail
        // while the metadata write would still succeed.
        std::fs::create_dir(store.directory().join(DOCUMENT_TEMP)).expect("occupy the temp path");

        let error = store
            .write(DOC_B, &meta_for(DOC_B, 2))
            .await
            .expect_err("the temporary document cannot be created");
        assert!(
            matches!(error, CacheError::Io { .. }),
            "expected Io, got {error:?}"
        );

        assert_eq!(
            std::fs::read(store.directory().join(META_FILE)).expect("meta is still readable"),
            previous_meta,
            "a failed write must not touch the metadata of the last good entry"
        );
        let cached = store
            .read()
            .await
            .expect("the previous entry is still valid");
        assert_eq!(cached.bytes().as_ref(), DOC_A);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_reports_io_instead_of_panicking_when_the_directory_is_read_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let (directory, store) = store();
        std::fs::create_dir_all(store.directory()).expect("create the cache directory");
        let read_only = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(store.directory(), read_only).expect("make the cache read-only");

        let result = store.write(DOC_A, &meta_for(DOC_A, 1)).await;

        // Restore write permission first: the temporary directory cannot be removed otherwise.
        let writable = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(store.directory(), writable).expect("restore the permissions");
        drop(directory);

        assert!(
            matches!(result, Err(CacheError::Io { .. })),
            "expected an Io error, got {result:?}"
        );
    }
}
