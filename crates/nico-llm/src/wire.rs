//! Shared wire helpers: bytes and JSON, no protocol semantics.
//!
//! The adapters on both sides of the exchange parse the same documents and spell an inline image with
//! the same bytes, so the two rules that are identical everywhere live here instead of being copied
//! into each module:
//!
//! - [`base64`] is RFC 4648 in both directions, with a typed refusal for each way a payload can be
//!   malformed. One side writes an inline image as that text and the other reads it back.
//! - [`json`] parses a document without ever quoting its payload, classifies a serialization failure,
//!   and refuses a member a modelled object does not declare.
//!
//! Nothing here knows what a protocol or a vendor is. A helper that needs to know which protocol
//! asked belongs in that protocol's module; this module is scanned as vendor-free by
//! `tests/structure.rs`, the same way the canonical model and the exchange are.

pub(crate) mod base64;
pub(crate) mod json;
