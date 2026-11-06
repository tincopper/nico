//! Last-known-good cache for the published document.
//!
//! The cache is a fallback, never a source of truth. It exists so a process whose configuration
//! source is unavailable can still start when the operator explicitly allows it, and so a restart
//! after a bad edit has something to compare against.
//!
//! Two files in one directory:
//!
//! - `last-good.yaml` — the document exactly as published.
//! - `last-good.meta.yaml` — revision, digest, generation, write time and source name.
//!
//! Both are written through a temporary file, `fsync`ed and renamed, so a crash never leaves a
//! half-written document. The two renames are not atomic as a pair: a crash between them leaves a
//! new document next to the old metadata, which [`CacheStore::read`] reports as
//! [`CacheError::Corrupt`]. Reporting corruption is the safe direction — the alternative would be
//! serving a snapshot whose metadata describes a different document.

use std::{
    io,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use nico_core::{ConfigDigest, ErrorCategory};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// File name of the cached document.
const DOCUMENT_FILE: &str = "last-good.yaml";
/// File name of the cached metadata.
const META_FILE: &str = "last-good.meta.yaml";
/// Temporary file used while writing the document.
const DOCUMENT_TEMP: &str = "last-good.yaml.tmp";
/// Temporary file used while writing the metadata.
const META_TEMP: &str = "last-good.meta.yaml.tmp";

/// Metadata stored next to the cached document.
///
/// Stored as YAML so the cache needs no second serialization format. Unknown fields are rejected: a
/// metadata file written by a different version must be treated as unreadable rather than silently
/// partially understood.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMeta {
    revision_seq: u64,
    digest_hex: String,
    generation: u64,
    written_at_unix_secs: u64,
    source: String,
}

impl CacheMeta {
    /// Creates metadata for a snapshot.
    #[must_use]
    pub fn new(
        revision_seq: u64,
        digest_hex: impl Into<String>,
        generation: u64,
        written_at_unix_secs: u64,
        source: impl Into<String>,
    ) -> Self {
        Self {
            revision_seq,
            digest_hex: digest_hex.into(),
            generation,
            written_at_unix_secs,
            source: source.into(),
        }
    }

    /// Returns the source-scoped revision sequence the document was read at.
    #[must_use]
    pub const fn revision_seq(&self) -> u64 {
        self.revision_seq
    }

    /// Returns the hex digest of the cached document.
    #[must_use]
    pub fn digest_hex(&self) -> &str {
        &self.digest_hex
    }

    /// Returns the generation the document was published as.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the Unix timestamp, in seconds, of the cache write.
    #[must_use]
    pub const fn written_at_unix_secs(&self) -> u64 {
        self.written_at_unix_secs
    }

    /// Returns the name of the source that produced the document.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A cache entry that passed metadata and digest validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSnapshot {
    meta: CacheMeta,
    bytes: Bytes,
}

impl CachedSnapshot {
    /// Returns the validated metadata.
    #[must_use]
    pub const fn meta(&self) -> &CacheMeta {
        &self.meta
    }

    /// Returns the cached document bytes.
    #[must_use]
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// Why the cache could not be written or read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CacheError {
    /// The cache directory or one of its files could not be read or written.
    ///
    /// A missing cache is reported this way too, with [`io::ErrorKind::NotFound`], so a caller can
    /// distinguish "no fallback yet" from "the fallback is damaged".
    #[error("cache file system error: {source}")]
    Io {
        /// Underlying file-system error.
        #[from]
        source: io::Error,
    },
    /// The cache exists but its metadata or document cannot be trusted.
    #[error("cache content is corrupt: {reason}")]
    Corrupt {
        /// What failed the check. Never contains document content.
        reason: &'static str,
    },
}

impl CacheError {
    /// Classifies the failure for the edge layer.
    ///
    /// A file-system failure is a failed dependency; damaged cache content is an internal state
    /// problem, because the cache is written by this process.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Io { .. } => ErrorCategory::Upstream,
            Self::Corrupt { .. } => ErrorCategory::Internal,
        }
    }
}

/// Errors compare by value so tests can assert the exact variant.
///
/// [`io::Error`] is not comparable, so [`CacheError::Io`] compares the error kind and the message:
/// enough to assert "permission denied" without weakening the type.
impl PartialEq for CacheError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Io { source: left }, Self::Io { source: right }) => {
                left.kind() == right.kind() && left.to_string() == right.to_string()
            }
            (Self::Corrupt { reason: left }, Self::Corrupt { reason: right }) => left == right,
            _ => false,
        }
    }
}

impl Eq for CacheError {}

/// A last-known-good cache rooted at one directory.
///
/// One directory has one writer: the temporary file names are fixed, so two processes sharing a
/// cache directory would overwrite each other's in-flight writes. A cache is per-process state and
/// belongs in a path the deployment gives to exactly one gateway instance.
#[derive(Debug, Clone)]
pub struct CacheStore {
    directory: PathBuf,
}

impl CacheStore {
    /// Creates a store that reads and writes below `directory`.
    ///
    /// The directory is created on the first write, so a store can be constructed before the
    /// deployment has provisioned anything.
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Returns the cache directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Writes the document and its metadata atomically.
    ///
    /// Both files are written to a temporary sibling, flushed, and renamed. A directory `sync` is
    /// attempted afterwards as a best-effort durability step and never fails the write.
    ///
    /// # Errors
    /// Returns [`CacheError::Io`] when the directory cannot be created or a file cannot be written.
    /// The previously cached entry is left untouched in that case.
    pub async fn write(&self, bytes: &[u8], meta: &CacheMeta) -> Result<(), CacheError> {
        tokio::fs::create_dir_all(&self.directory).await?;

        let encoded = serde_norway::to_string(meta).map_err(|error| CacheError::Io {
            source: io::Error::other(format!("cache metadata could not be encoded: {error}")),
        })?;

        let document = self.directory.join(DOCUMENT_FILE);
        let metadata = self.directory.join(META_FILE);
        let document_temp = self.directory.join(DOCUMENT_TEMP);
        let metadata_temp = self.directory.join(META_TEMP);

        // Encode both temporary files first, so a failure can only leave the old pair in place.
        write_flushed(&document_temp, bytes).await?;
        write_flushed(&metadata_temp, encoded.as_bytes()).await?;
        tokio::fs::rename(&document_temp, &document).await?;
        tokio::fs::rename(&metadata_temp, &metadata).await?;

        sync_directory(&self.directory).await;
        Ok(())
    }

    /// Reads and validates the cached entry.
    ///
    /// # Errors
    /// Returns [`CacheError::Io`] when a file is missing or unreadable, and [`CacheError::Corrupt`]
    /// when the metadata cannot be parsed, declares generation `0`, or does not describe the
    /// document that is on disk.
    pub async fn read(&self) -> Result<CachedSnapshot, CacheError> {
        let metadata_bytes = tokio::fs::read(self.directory.join(META_FILE)).await?;
        let meta: CacheMeta =
            serde_norway::from_slice(&metadata_bytes).map_err(|_error| CacheError::Corrupt {
                reason: "cache metadata is not valid YAML",
            })?;

        if meta.generation == 0 {
            return Err(CacheError::Corrupt {
                reason: "cached snapshot has no generation",
            });
        }

        let bytes = tokio::fs::read(self.directory.join(DOCUMENT_FILE)).await?;
        if ConfigDigest::of(&bytes).hex() != meta.digest_hex {
            return Err(CacheError::Corrupt {
                reason: "cached document does not match its digest",
            });
        }

        Ok(CachedSnapshot {
            meta,
            bytes: Bytes::from(bytes),
        })
    }
}

/// Writes `bytes` and flushes them to the device, leaving the file closed.
async fn write_flushed(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    use tokio::io::AsyncWriteExt as _;

    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

/// Persists the directory entry itself, so the renames survive a crash.
///
/// Failure is not an error: the snapshot is already published, and the cache is a best-effort
/// fallback whose loss costs one restart's worth of freshness.
async fn sync_directory(directory: &Path) {
    match tokio::fs::File::open(directory).await {
        Ok(handle) => {
            if let Err(error) = handle.sync_all().await {
                tracing::debug!(error = %error, "cache directory sync failed");
            }
        }
        Err(error) => {
            tracing::debug!(error = %error, "cache directory could not be opened for sync");
        }
    }
}

#[cfg(test)]
mod tests;
