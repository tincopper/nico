//! Reading a file under a byte ceiling.
//!
//! A bound that is checked before the read is not a bound. `metadata` and `open` are two syscalls on
//! a *name*, and the name may be replaced between them — and it may not address a regular file at
//! all: a FIFO reports length zero, a device node reports whatever it likes, and `metadata` follows a
//! symlink into either. So the ceiling is applied **by the read** (`take(limit + 1)`), and the one
//! pre-read check is about the kind of thing the name addresses, because that decides whether opening
//! it can block forever or never end.
//!
//! Two callers share this and differ only in what they do with the bytes: the local configuration
//! source (a document) and the credential resolver (a key file). They must not differ in how many of
//! them the process is willing to hold, which is why the rule lives here rather than in each of them.
//!
//! The residual window is narrow and deliberate: a path swapped for a FIFO *after* the kind check
//! parks one blocking thread in `open(2)`. Closing it needs `O_NONBLOCK`, which is a platform flag this
//! workspace does not take a dependency on; the memory bound — the failure that takes the process down
//! — is closed by the read itself.

use std::{fs, io::Read as _, path::Path};

use thiserror::Error;

/// Why a bounded read could not return content.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The path does not name a regular file, so its length says nothing about what reading it costs.
    #[error("the path does not name a regular file")]
    NotRegular,
    /// The content is over the ceiling.
    ///
    /// `at_least` is `limit + 1`: the reader stops one byte past the ceiling rather than buffering the
    /// rest to learn the exact size, so the real length is only known to be larger than `limit`.
    #[error("the content exceeds the {limit} byte limit (it is at least {at_least} bytes)")]
    TooLarge {
        /// Smallest length the content is known to have: one byte more than the ceiling.
        at_least: usize,
        /// The ceiling the caller set.
        limit: usize,
    },
    /// The filesystem refused.
    #[error("the file cannot be read")]
    Io(#[source] std::io::Error),
}

/// Reads `path` as a regular file, refusing to hold more than `limit` bytes of it.
///
/// # Errors
/// Returns [`ReadError::NotRegular`] when the path names something other than a regular file,
/// [`ReadError::TooLarge`] when the content is over `limit`, and [`ReadError::Io`] for anything the
/// filesystem reports. The `Io` cause is a source, never interpolated: a path can name a place a
/// credential lives.
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, ReadError> {
    let metadata = fs::metadata(path).map_err(ReadError::Io)?;
    if !metadata.is_file() {
        return Err(ReadError::NotRegular);
    }

    let file = fs::File::open(path).map_err(ReadError::Io)?;
    // One byte past the ceiling is enough to answer "too large" without reading the rest.
    let ceiling = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    file.take(ceiling)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() > limit {
        return Err(ReadError::TooLarge {
            at_least: bytes.len(),
            limit,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
